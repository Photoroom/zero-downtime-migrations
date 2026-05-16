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

impl Diagnostic {
    /// Create a new diagnostic with no help text. Chain `.with_help(...)`
    /// to attach a suggestion.
    pub fn new(
        rule_id: &'static str,
        rule_name: &'static str,
        severity: Severity,
        message: impl Into<String>,
        path: impl Into<PathBuf>,
        span: Span,
    ) -> Self {
        Self {
            rule_id,
            rule_name,
            severity,
            message: message.into(),
            path: path.into(),
            span,
            help: None,
        }
    }

    /// Attach help text to this diagnostic.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}
