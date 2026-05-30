//! Severity levels for diagnostics.

/// The severity level of a diagnostic.
///
/// Not `#[non_exhaustive]` — the two-variant set has been stable
/// for the project's lifetime and downstream code legitimately
/// wants to match exhaustively (e.g. mapping to log levels). If
/// a third level is ever introduced, that's a breaking change
/// worth surfacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A warning that should be reviewed but doesn't block CI.
    Warning,
    /// An error that blocks CI (exit code 1).
    Error,
}

impl Severity {
    /// The lowercase label used in human and JSON output
    /// (`"error"` / `"warning"`). Avoids leaking the `Debug`
    /// representation into user-facing text.
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}
