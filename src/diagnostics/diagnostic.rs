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
    /// Optional fix that could be applied (for future --fix support).
    pub fix: Option<Fix>,
}

/// A potential fix for a diagnostic.
///
/// This is designed for future `--fix` support. The fix is stored
/// even if auto-fix is not yet implemented, allowing us to add
/// it later without changing the diagnostic structure.
#[derive(Debug, Clone)]
pub struct Fix {
    /// Description of what the fix does.
    pub description: String,
    /// The span to replace.
    pub span: Span,
    /// The replacement text.
    pub replacement: String,
}

impl Diagnostic {
    /// Create a new diagnostic.
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
            fix: None,
        }
    }

    /// Create an error diagnostic.
    pub fn error(
        rule_id: &'static str,
        rule_name: &'static str,
        message: impl Into<String>,
        path: impl Into<PathBuf>,
        span: Span,
    ) -> Self {
        Self::new(rule_id, rule_name, Severity::Error, message, path, span)
    }

    /// Create a warning diagnostic.
    pub fn warning(
        rule_id: &'static str,
        rule_name: &'static str,
        message: impl Into<String>,
        path: impl Into<PathBuf>,
        span: Span,
    ) -> Self {
        Self::new(rule_id, rule_name, Severity::Warning, message, path, span)
    }

    /// Add help text to this diagnostic.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Add a fix to this diagnostic.
    pub fn with_fix(mut self, fix: Fix) -> Self {
        self.fix = Some(fix);
        self
    }

    /// Returns true if this diagnostic is an error.
    pub fn is_error(&self) -> bool {
        self.severity.is_error()
    }

    /// Returns true if this diagnostic is a warning.
    pub fn is_warning(&self) -> bool {
        self.severity.is_warning()
    }
}

impl Fix {
    /// Create a new fix.
    pub fn new(description: impl Into<String>, span: Span, replacement: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            span,
            replacement: replacement.into(),
        }
    }
}

/// A collection of diagnostics from linting.
#[derive(Debug, Default)]
pub struct Diagnostics {
    diagnostics: Vec<Diagnostic>,
}

impl Diagnostics {
    /// Create a new empty diagnostics collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a diagnostic to the collection.
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Extend with diagnostics from another collection.
    pub fn extend(&mut self, other: impl IntoIterator<Item = Diagnostic>) {
        self.diagnostics.extend(other);
    }

    /// Returns true if there are any diagnostics.
    pub fn has_any(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Returns true if there are any error diagnostics.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.is_error())
    }

    /// Returns true if there are any warning diagnostics.
    pub fn has_warnings(&self) -> bool {
        self.diagnostics.iter().any(|d| d.is_warning())
    }

    /// Count of error diagnostics.
    pub fn error_count(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.is_error()).count()
    }

    /// Count of warning diagnostics.
    pub fn warning_count(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.is_warning()).count()
    }

    /// Total count of diagnostics.
    pub fn len(&self) -> usize {
        self.diagnostics.len()
    }

    /// Returns true if empty.
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Get an iterator over diagnostics.
    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter()
    }

    /// Consume and return the inner vector.
    pub fn into_inner(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    /// Sort diagnostics by file path and then by line number.
    pub fn sort(&mut self) {
        self.diagnostics.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.span.start_line.cmp(&b.span.start_line))
                .then_with(|| a.span.start_column.cmp(&b.span.start_column))
        });
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.diagnostics.into_iter()
    }
}

impl<'a> IntoIterator for &'a Diagnostics {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.diagnostics.iter()
    }
}
