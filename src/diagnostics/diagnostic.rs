//! The main Diagnostic type representing a lint violation.

use std::path::PathBuf;

use super::{Severity, Span};

/// A diagnostic representing a lint violation.
///
/// Each diagnostic contains all the information needed to display
/// a rich error message to the user.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// The rule ID (e.g., "R001").
    pub rule_id: &'static str,
    /// Short rule name for display.
    pub rule_name: &'static str,
    /// The severity of this diagnostic.
    pub severity: Severity,
    /// The primary message describing the issue.
    pub message: String,
    /// The source file path.
    pub path: PathBuf,
    /// The span in the source file.
    pub span: Span,
    /// Optional help text with suggestions for fixing.
    pub help: Option<String>,
}
