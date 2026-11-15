//! Targets: the importer under test. A target is reset, given each bundle of
//! a case in turn, then observed. recordchaos never interprets clinical
//! meaning: the view is whatever JSON the target returns, read at the
//! declared pointers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

pub mod command;
pub mod http;
pub mod python;

/// How one step (reset, ingest or observe) ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Exit 0 or HTTP 2xx.
    Accepted,
    /// A rejected exit code or HTTP 4xx.
    Rejected,
    /// Any other exit code, a signal, HTTP 5xx or a transport error.
    Crashed,
    /// No answer within the timeout.
    Timeout,
}

/// Outcome of one step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub kind: StepKind,
    /// Exit code for command targets, HTTP status for HTTP targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    /// True for HTTP targets.
    #[serde(default)]
    pub http: bool,
    /// First non-empty line of stderr (command) or the response body (HTTP).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_line: String,
    /// True when an HTTP 4xx body was an OperationOutcome.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub operation_outcome: bool,
    /// Wall time. Never written to results.json, which must be deterministic.
    #[serde(skip)]
    pub elapsed_ms: u64,
}

impl StepOutcome {
    pub fn accepted() -> Self {
        Self {
            kind: StepKind::Accepted,
            code: Some(0),
            http: false,
            first_line: String::new(),
            operation_outcome: false,
            elapsed_ms: 0,
        }
    }

    pub fn crashed(first_line: impl Into<String>) -> Self {
        Self {
            kind: StepKind::Crashed,
            code: None,
            http: false,
            first_line: first_line.into(),
            operation_outcome: false,
            elapsed_ms: 0,
        }
    }

    /// Human text: "exit 101", "HTTP 500", "timed out".
    pub fn code_text(&self) -> String {
        match (self.kind, self.code, self.http) {
            (StepKind::Timeout, _, _) => String::from("timed out"),
            (_, Some(c), true) => format!("HTTP {c}"),
            (_, Some(c), false) => format!("exit {c}"),
            (_, None, true) => String::from("no HTTP response"),
            (_, None, false) => String::from("killed by a signal"),
        }
    }

    /// Fingerprint part: exit code, or the HTTP status class.
    pub fn class(&self) -> String {
        match (self.kind, self.code, self.http) {
            (StepKind::Timeout, _, _) => String::from("timeout"),
            (_, Some(c), true) => format!("http:{}xx", c / 100),
            (_, Some(c), false) => format!("exit:{c}"),
            (_, None, true) => String::from("http:none"),
            (_, None, false) => String::from("signal"),
        }
    }

    pub fn is_crash(&self) -> bool {
        matches!(self.kind, StepKind::Crashed | StepKind::Timeout)
    }
}

/// Observed view: declared name to value. Absent pointers are left out.
pub type View = BTreeMap<String, Value>;

/// One declared view entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewEntry {
    /// JSON Pointer into the observed document.
    pub pointer: String,
    /// For HTTP targets: the GET path, relative to base_url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub get: Option<String>,
}

impl ViewEntry {
    /// Text that oracles search for a code: the pointer plus the GET path.
    pub fn locator(&self) -> String {
        match &self.get {
            Some(g) => format!("{g} {}", self.pointer),
            None => self.pointer.clone(),
        }
    }
}

/// How the target reports acceptance.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptRules {
    /// Exit codes that mean "rejected cleanly" (command targets).
    #[serde(default)]
    pub rejected_exit_codes: Vec<i32>,
    /// A view name whose truthy value means "accepted but flagged".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag_view: Option<String>,
}

/// What happened when a case ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Execution {
    pub reset: StepOutcome,
    pub ingests: Vec<StepOutcome>,
    pub observe: Option<StepOutcome>,
    pub view: View,
}

impl Execution {
    /// The step that decides the outcome: the first step that did not
    /// succeed, else the last ingest.
    pub fn deciding(&self) -> &StepOutcome {
        if self.reset.kind != StepKind::Accepted {
            return &self.reset;
        }
        for s in &self.ingests {
            if s.kind != StepKind::Accepted {
                return s;
            }
        }
        if let Some(o) = &self.observe {
            if o.kind != StepKind::Accepted {
                return o;
            }
        }
        self.ingests.last().unwrap_or(&self.reset)
    }

    pub fn crashed(&self) -> bool {
        self.deciding().is_crash()
    }

    pub fn rejected(&self) -> bool {
        self.deciding().kind == StepKind::Rejected
    }

    pub fn accepted(&self) -> bool {
        self.deciding().kind == StepKind::Accepted
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.reset.elapsed_ms
            + self.ingests.iter().map(|s| s.elapsed_ms).sum::<u64>()
            + self.observe.as_ref().map(|s| s.elapsed_ms).unwrap_or(0)
    }
}

/// An importer under test.
pub trait Target {
    /// One line for the terminal and report, e.g. the base URL or command.
    fn describe(&self) -> String;
    /// The declared view entries.
    fn view_spec(&self) -> &BTreeMap<String, ViewEntry>;
    /// Acceptance rules.
    fn accept(&self) -> &AcceptRules;
    /// Clear the target's state. `Err` means the target is unavailable.
    fn reset(&mut self) -> Result<StepOutcome>;
    /// Give the target one bundle. `Err` means the target is unavailable.
    fn ingest(&mut self, bundle: &Value) -> Result<StepOutcome>;
    /// Read the view. `Err` means the target is unavailable.
    fn observe(&mut self) -> Result<(StepOutcome, View)>;
}

/// Run one case: reset, ingest each step in order, then observe when every
/// ingest was accepted.
pub fn execute(target: &mut dyn Target, steps: &[&Value]) -> Result<Execution> {
    let reset = target.reset()?;
    let mut exec = Execution {
        reset,
        ingests: Vec::new(),
        observe: None,
        view: View::new(),
    };
    if exec.reset.kind != StepKind::Accepted {
        return Ok(exec);
    }
    for step in steps {
        let out = target.ingest(step)?;
        let ok = out.kind == StepKind::Accepted;
        exec.ingests.push(out);
        if !ok {
            return Ok(exec);
        }
    }
    let (obs, view) = target.observe()?;
    exec.observe = Some(obs);
    exec.view = view;
    Ok(exec)
}

/// Read every declared pointer from an observed document.
pub fn extract_view(doc: &Value, spec: &BTreeMap<String, ViewEntry>) -> View {
    let mut view = View::new();
    for (name, entry) in spec {
        if let Some(v) = doc.pointer(&entry.pointer) {
            view.insert(name.clone(), v.clone());
        }
    }
    view
}

/// First non-empty line of some output, trimmed, with run-specific values
/// removed (see [`strip_volatile`]) and capped at 200 characters.
pub fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    strip_volatile(line).chars().take(200).collect()
}

/// Remove values that change on every run, so the line, and the fingerprint
/// hashed from it, stay the same for the same failure. Rust 1.88 and later
/// print the OS thread id in panic messages:
/// `thread 'main' (19556) panicked at ...` becomes `thread 'main' panicked at ...`.
fn strip_volatile(line: &str) -> String {
    let line = strip_thread_id(line);
    // The panic location uses the OS path separator; use / everywhere so the
    // same panic has the same fingerprint on Windows, macOS and Linux.
    match line.split_once(" panicked at ") {
        Some((head, place)) if head.starts_with("thread '") => {
            format!("{head} panicked at {}", place.replace('\\', "/"))
        }
        _ => line,
    }
}

fn strip_thread_id(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("thread '") {
        if let Some(name_end) = rest.find('\'') {
            let after = &rest[name_end + 1..];
            if let Some(inner) = after.strip_prefix(" (") {
                if let Some(close) = inner.find(')') {
                    let id = &inner[..close];
                    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                        return format!("thread '{}'{}", &rest[..name_end], &inner[close + 1..]);
                    }
                }
            }
        }
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::first_line;

    #[test]
    fn panic_paths_use_forward_slashes() {
        let w = first_line(br"thread 'main' (7) panicked at crates\cli\..\main.rs:164:13:");
        let l = first_line(b"thread 'main' panicked at crates/cli/../main.rs:164:13:");
        assert_eq!(w, l);
    }

    #[test]
    fn panic_thread_id_is_removed() {
        let a = first_line(b"\n  thread 'main' (19556) panicked at src/main.rs:164:13:\nboom");
        let b = first_line(b"thread 'main' (5096) panicked at src/main.rs:164:13:");
        assert_eq!(a, "thread 'main' panicked at src/main.rs:164:13:");
        assert_eq!(a, b);
    }

    #[test]
    fn other_lines_are_unchanged() {
        assert_eq!(
            first_line(b"thread 'main' panicked at x"),
            "thread 'main' panicked at x"
        );
        assert_eq!(
            first_line(b"thread 'w' (abc) panicked"),
            "thread 'w' (abc) panicked"
        );
        assert_eq!(
            first_line(b"Traceback (most recent call last):"),
            "Traceback (most recent call last):"
        );
    }
}
