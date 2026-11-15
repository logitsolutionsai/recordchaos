//! Terminal output, following the "Terminal Output" board: one line per stage
//! with the time at the right edge, problems last, the verdict just above the
//! file paths. Symbols carry the meaning (✗ failed, ▲ needs a decision,
//! ✓ passed); colour only adds to them. NO_COLOR and `--color never` are
//! honoured, and nothing animates.

use std::io::{IsTerminal, Write};

use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// Where the human-readable lines go: stdout normally, stderr with `--json`.
pub struct Term {
    color: bool,
    to_stderr: bool,
    quiet: bool,
}

const MUTED: (u8, u8, u8) = (0x8b, 0x97, 0xa8);
const FAIL: (u8, u8, u8) = (0xf9, 0x70, 0x66);
const CAUTION: (u8, u8, u8) = (0xfd, 0xb0, 0x22);
const LINK: (u8, u8, u8) = (0x5b, 0x9b, 0xff);

/// Width at which stage times are right-aligned.
const LINE_WIDTH: usize = 86;

impl Term {
    pub fn new(choice: ColorChoice, json: bool, quiet: bool) -> Self {
        let stream_is_tty = if json {
            std::io::stderr().is_terminal()
        } else {
            std::io::stdout().is_terminal()
        };
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let color = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => stream_is_tty && !no_color,
        };
        Self {
            color,
            to_stderr: json,
            quiet,
        }
    }

    fn paint(&self, rgb: (u8, u8, u8), text: &str, bold: bool) -> String {
        if !self.color {
            return text.to_string();
        }
        let weight = if bold { "1;" } else { "" };
        format!(
            "\x1b[{weight}38;2;{};{};{}m{text}\x1b[0m",
            rgb.0, rgb.1, rgb.2
        )
    }

    pub fn muted(&self, t: &str) -> String {
        self.paint(MUTED, t, false)
    }

    pub fn fail(&self, t: &str) -> String {
        self.paint(FAIL, t, false)
    }

    pub fn fail_bold(&self, t: &str) -> String {
        self.paint(FAIL, t, true)
    }

    pub fn caution(&self, t: &str) -> String {
        self.paint(CAUTION, t, false)
    }

    pub fn link(&self, t: &str) -> String {
        self.paint(LINK, t, false)
    }

    pub fn bold(&self, t: &str) -> String {
        if self.color {
            format!("\x1b[1m{t}\x1b[0m")
        } else {
            t.to_string()
        }
    }

    /// Print one line (unless quiet).
    pub fn line(&self, text: &str) {
        if self.quiet {
            return;
        }
        self.always(text);
    }

    /// Print one line even when quiet.
    pub fn always(&self, text: &str) {
        if self.to_stderr {
            let _ = writeln!(std::io::stderr(), "{text}");
        } else {
            let _ = writeln!(std::io::stdout(), "{text}");
        }
    }

    pub fn blank(&self) {
        self.line("");
    }

    /// `generate  25 cases from 4 operators            0.04 s`
    pub fn stage(&self, name: &str, text: &str, seconds: Option<f64>) {
        let label = format!("{name:<10}");
        let plain_len = label.chars().count() + text.chars().count();
        let mut out = format!("{}{text}", self.muted(&label));
        if let Some(s) = seconds {
            let time = format!("{s:.2} s");
            let pad = LINE_WIDTH.saturating_sub(plain_len + time.len()).max(2);
            out.push_str(&" ".repeat(pad));
            out.push_str(&self.muted(&time));
        }
        self.line(&out);
    }

    /// Error line on stderr, always shown.
    pub fn error(&self, text: &str) {
        let _ = writeln!(std::io::stderr(), "{} {text}", self.fail("✗"));
    }

    /// Warning line on stderr, always shown.
    pub fn warn(&self, text: &str) {
        let _ = writeln!(std::io::stderr(), "{} {text}", self.caution("▲"));
    }
}
