use std::fmt;
use std::path::PathBuf;

/// Severity of a configuration diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

/// A single finding produced while loading the configuration.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub location: Option<(PathBuf, usize)>,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.severity {
            Severity::Warning => "Warning",
            Severity::Error => "Error",
        };
        match &self.location {
            Some((path, line)) => write!(f, "{kind}: {} ({}:{line})", self.message, path.display()),
            None => write!(f, "{kind}: {}", self.message),
        }
    }
}

/// Fatal error that prevents the configuration from being used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}:{line}: {message}")]
    Syntax { path: PathBuf, line: usize, message: String },
    #[error("configuration contains {count} error(s):\n{details}")]
    Invalid { count: usize, details: String },
}
