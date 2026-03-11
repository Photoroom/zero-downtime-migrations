//! Severity levels for diagnostics.

use std::fmt;

/// The severity level of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// A warning that should be reviewed but doesn't block CI.
    Warning,
    /// An error that blocks CI (exit code 1).
    Error,
}

impl Severity {
    /// Returns true if this severity is an error.
    pub fn is_error(&self) -> bool {
        matches!(self, Severity::Error)
    }

    /// Returns true if this severity is a warning.
    pub fn is_warning(&self) -> bool {
        matches!(self, Severity::Warning)
    }

    /// Returns the ANSI color code for this severity.
    pub fn color(&self) -> &'static str {
        match self {
            Severity::Warning => "yellow",
            Severity::Error => "red",
        }
    }

    /// Returns the display label for this severity.
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}
