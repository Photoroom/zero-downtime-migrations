//! Severity levels for diagnostics.

/// The severity level of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A warning that should be reviewed but doesn't block CI.
    Warning,
    /// An error that blocks CI (exit code 1).
    Error,
}
