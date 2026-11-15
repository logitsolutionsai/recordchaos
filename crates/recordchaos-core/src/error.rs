//! Errors and their exit codes.
//!
//! Exit codes are shared with promochaos and the catalog checker:
//! 0 no failures, 1 failures found, 2 invalid input or config,
//! 3 target or validator unavailable, 4 internal error.

use std::fmt;

/// Exit code: no failures.
pub const EXIT_CLEAN: i32 = 0;
/// Exit code: failures found.
pub const EXIT_FAILURES: i32 = 1;
/// Exit code: invalid input or config.
pub const EXIT_INPUT: i32 = 2;
/// Exit code: target or validator unavailable.
pub const EXIT_UNAVAILABLE: i32 = 3;
/// Exit code: internal error.
pub const EXIT_INTERNAL: i32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Invalid input or config (exit 2).
    Input(String),
    /// Target or validator unavailable (exit 3).
    Unavailable(String),
    /// Internal error (exit 4).
    Internal(String),
}

impl Error {
    pub fn input(msg: impl Into<String>) -> Self {
        Error::Input(msg.into())
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Error::Unavailable(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Error::Internal(msg.into())
    }

    /// The process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Input(_) => EXIT_INPUT,
            Error::Unavailable(_) => EXIT_UNAVAILABLE,
            Error::Internal(_) => EXIT_INTERNAL,
        }
    }

    /// The message without the kind prefix.
    pub fn message(&self) -> &str {
        match self {
            Error::Input(m) | Error::Unavailable(m) | Error::Internal(m) => m,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Input(m) => write!(f, "invalid input: {m}"),
            Error::Unavailable(m) => write!(f, "unavailable: {m}"),
            Error::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Read a file as text, mapping failures to an input error that names the path.
pub fn read_text(path: &std::path::Path, max_bytes: usize) -> Result<String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Error::input(format!("cannot read {}: {e}", path.display())))?;
    if meta.len() > max_bytes as u64 {
        return Err(Error::input(format!(
            "{} is {} bytes, over the limit of {} bytes",
            path.display(),
            meta.len(),
            max_bytes
        )));
    }
    std::fs::read_to_string(path)
        .map_err(|e| Error::input(format!("cannot read {}: {e}", path.display())))
}

/// Write a file, creating parent folders. A folder or file that cannot be
/// written is the user's input (a bad `--out`), so it maps to exit 2.
pub fn write_text(path: &std::path::Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::input(format!("cannot create {}: {e}", parent.display())))?;
        }
    }
    std::fs::write(path, text)
        .map_err(|e| Error::input(format!("cannot write {}: {e}", path.display())))
}

/// A TOML parse error as one line: `file line L, column C: message`.
/// The toml crate's own Display spans several lines with a source excerpt.
pub fn toml_error(file: &str, text: &str, e: &toml::de::Error) -> Error {
    let msg = e.message().split_whitespace().collect::<Vec<_>>().join(" ");
    match e.span().and_then(|s| text.get(..s.start)) {
        Some(upto) => {
            let line = upto.matches('\n').count() + 1;
            let col = upto.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            Error::input(format!("{file} line {line}, column {col}: {msg}"))
        }
        None => Error::input(format!("{file}: {msg}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_errors_are_one_line_with_position() {
        let text = "a = 1\n[[operator\nid = 2\n";
        let e = toml::from_str::<toml::Table>(text).unwrap_err();
        let err = toml_error("s.toml", text, &e);
        assert_eq!(err.exit_code(), EXIT_INPUT);
        assert!(!err.message().contains('\n'), "{}", err.message());
        assert!(
            err.message().starts_with("s.toml line 2, column "),
            "{}",
            err.message()
        );
    }

    #[test]
    fn unwritable_output_is_an_input_error() {
        let dir = std::env::temp_dir().join(format!("rc-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A file where a folder is needed.
        let file = dir.join("not-a-folder");
        std::fs::write(&file, "x").unwrap();
        let err = write_text(&file.join("results.json"), "{}").unwrap_err();
        assert_eq!(err.exit_code(), EXIT_INPUT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
