//! The main Diagnostic type representing a lint violation.

use std::path::PathBuf;

use super::{Severity, Span};

/// A diagnostic representing a lint violation.
///
/// Each diagnostic contains all the information needed to display
/// a rich error message to the user.
///
/// Build via [`Diagnostic::new`]. Optional fields use the
/// `with_*` builder pattern. The type is `#[non_exhaustive]`
/// to reserve room for new fields, which means out-of-tree code
/// cannot struct-literal it — use the constructor.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// Construct a diagnostic with the required fields. Use
    /// [`Self::with_help`] to attach help text.
    pub fn new(
        rule_id: &'static str,
        rule_name: &'static str,
        severity: Severity,
        message: impl Into<String>,
        path: PathBuf,
        span: Span,
    ) -> Self {
        Self {
            rule_id,
            rule_name,
            severity,
            message: message.into(),
            path,
            span,
            help: None,
        }
    }

    /// Attach help text — typically a sentence or two telling
    /// the user how to fix the violation.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}
