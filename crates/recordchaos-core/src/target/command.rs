//! Command targets: any program, run without a shell, bundle on stdin and the
//! view as JSON on stdout.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    AcceptRules, StepKind, StepOutcome, Target, View, ViewEntry, extract_view, first_line,
};
use crate::error::{Error, Result};

/// What a finished process produced.
#[derive(Clone, Debug)]
pub struct ProcessOutput {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed_ms: u64,
}

fn read_capped<R: Read>(mut reader: R, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Keep draining past the cap so the child never blocks on a full pipe.
                let room = max.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    out
}

/// Run a program without a shell, with a timeout and capped output.
/// `Err` means the program could not be started.
pub fn run_process(
    argv: &[String],
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    max_stdout: usize,
) -> io::Result<ProcessOutput> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let writer = match (child.stdin.take(), stdin) {
        (Some(mut pipe), Some(data)) => Some(thread::spawn(move || {
            // A child that exits early closes the pipe; that is not our error.
            let _ = pipe.write_all(&data);
        })),
        _ => None,
    };
    let out_reader = child
        .stdout
        .take()
        .map(|s| thread::spawn(move || read_capped(s, max_stdout)));
    let err_reader = child
        .stderr
        .take()
        .map(|s| thread::spawn(move || read_capped(s, 64 * 1024)));
    let deadline = start + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        thread::sleep(Duration::from_millis(2));
    };
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    // After a timeout a grandchild may still hold the pipes open, so the
    // readers are not joined; they end when the pipes close.
    let (stdout, stderr) = if timed_out {
        (Vec::new(), Vec::new())
    } else {
        let o = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
        let e = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
        (o, e)
    };
    if let Some(w) = writer {
        if !timed_out {
            let _ = w.join();
        }
    }
    Ok(ProcessOutput {
        code: status.and_then(|s| s.code()),
        timed_out,
        stdout,
        stderr,
        elapsed_ms,
    })
}

/// A command target's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandConfig {
    pub reset: Option<Vec<String>>,
    pub ingest: Vec<String>,
    pub observe: Vec<String>,
    pub timeout_ms: u64,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub accept: AcceptRules,
    pub view: BTreeMap<String, ViewEntry>,
    pub max_view_bytes: usize,
}

/// A target that runs commands.
pub struct CommandTarget {
    cfg: CommandConfig,
}

impl CommandTarget {
    pub fn new(cfg: CommandConfig) -> Self {
        Self { cfg }
    }

    fn run(
        &self,
        argv: &[String],
        stdin: Option<Vec<u8>>,
        what: &str,
    ) -> Result<(StepOutcome, Vec<u8>)> {
        let out = run_process(
            argv,
            stdin,
            Duration::from_millis(self.cfg.timeout_ms.max(1)),
            self.cfg.cwd.as_deref(),
            &self.cfg.env,
            self.cfg.max_view_bytes,
        )
        .map_err(|e| {
            Error::unavailable(format!(
                "cannot start the {what} command {:?}: {e}. Build the importer first, or fix the path in the target file.",
                argv.first().map(String::as_str).unwrap_or("")
            ))
        })?;
        let kind = if out.timed_out {
            StepKind::Timeout
        } else {
            match out.code {
                Some(0) => StepKind::Accepted,
                Some(c) if self.cfg.accept.rejected_exit_codes.contains(&c) => StepKind::Rejected,
                _ => StepKind::Crashed,
            }
        };
        let mut line = first_line(&out.stderr);
        if line.is_empty() && kind != StepKind::Accepted {
            line = first_line(&out.stdout);
        }
        if kind == StepKind::Timeout {
            line = format!("no exit within {} ms", self.cfg.timeout_ms);
        }
        let step = StepOutcome {
            kind,
            code: out.code.map(i64::from),
            http: false,
            first_line: line,
            operation_outcome: false,
            elapsed_ms: out.elapsed_ms,
        };
        Ok((step, out.stdout))
    }
}

impl Target for CommandTarget {
    fn describe(&self) -> String {
        self.cfg.ingest.join(" ")
    }

    fn view_spec(&self) -> &BTreeMap<String, ViewEntry> {
        &self.cfg.view
    }

    fn accept(&self) -> &AcceptRules {
        &self.cfg.accept
    }

    fn reset(&mut self) -> Result<StepOutcome> {
        match self.cfg.reset.clone() {
            Some(argv) => self.run(&argv, None, "reset").map(|(s, _)| s),
            None => Ok(StepOutcome::accepted()),
        }
    }

    fn ingest(&mut self, bundle: &Value) -> Result<StepOutcome> {
        let text = crate::json::canonical(bundle)?;
        let argv = self.cfg.ingest.clone();
        self.run(&argv, Some(text.into_bytes()), "ingest")
            .map(|(s, _)| s)
    }

    fn observe(&mut self) -> Result<(StepOutcome, View)> {
        let argv = self.cfg.observe.clone();
        let (mut step, stdout) = self.run(&argv, None, "observe")?;
        if step.kind != StepKind::Accepted {
            return Ok((step, View::new()));
        }
        if stdout.len() >= self.cfg.max_view_bytes {
            step.kind = StepKind::Crashed;
            step.first_line = format!(
                "observe printed more than {} bytes",
                self.cfg.max_view_bytes
            );
            return Ok((step, View::new()));
        }
        match serde_json::from_slice::<Value>(&stdout) {
            Ok(doc) => Ok((step, extract_view(&doc, &self.cfg.view))),
            Err(e) => {
                step.kind = StepKind::Crashed;
                step.first_line = format!("observe printed invalid JSON: {e}");
                Ok((step, View::new()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_program_is_an_error() {
        let r = run_process(
            &[String::from("recordchaos-no-such-program-xyz")],
            None,
            Duration::from_secs(1),
            None,
            &BTreeMap::new(),
            1024,
        );
        assert!(r.is_err());
    }

    #[test]
    fn empty_command_is_an_error() {
        assert!(
            run_process(
                &[],
                None,
                Duration::from_secs(1),
                None,
                &BTreeMap::new(),
                1024
            )
            .is_err()
        );
    }
}
