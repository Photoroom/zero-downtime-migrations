//! Error types for zero-downtime-migrations.
//!
//! This module defines a unified error type that covers all failure modes:
//! - File I/O errors
//! - Parse errors (tree-sitter)
//! - Configuration errors
//! - Git errors
//!
//! The error recovery strategy is:
//! - Continue on single file parse failure (report error, skip file)
//! - Abort on config parse failure (cannot proceed without config)
//! - Continue on git errors in non-diff mode (fall back to linting all files)

use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

/// A specialized Result type for zdm operations.
pub type Result<T> = std::result::Result<T, Error>;

/// The main error type for zdm.
#[derive(Error, Debug, Diagnostic)]
pub enum Error {
    /// File I/O error
    #[error("Failed to read file: {path}")]
    #[diagnostic(code(zdm::io::read_error))]
    FileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Directory walk error
    #[error("Failed to walk directory: {path}")]
    #[diagnostic(code(zdm::io::walk_error))]
    DirectoryWalk {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },

    /// File too large to process
    #[error("File too large: {path} ({size} bytes, max {max_size} bytes)")]
    #[diagnostic(
        code(zdm::io::file_too_large),
        help("Migration files should be small; this may indicate malformed input")
    )]
    FileTooLarge {
        path: PathBuf,
        size: u64,
        max_size: u64,
    },

    /// Tree-sitter parse error
    #[error("Failed to parse Python file: {path}: {message}")]
    #[diagnostic(
        code(zdm::parse::python_error),
        help("Ensure the file is valid Python syntax")
    )]
    ParseError { path: PathBuf, message: String },

    /// Tree-sitter parse error with location
    #[error("Parse error in {path} at line {line}, column {column}")]
    #[diagnostic(code(zdm::parse::syntax_error))]
    ParseErrorWithLocation {
        path: PathBuf,
        line: usize,
        column: usize,
    },

    /// Configuration file not found
    #[error("Configuration file not found: {path}")]
    #[diagnostic(
        code(zdm::config::not_found),
        help("Create a pyproject.toml with [tool.zdm] or zero-downtime-migrations.toml")
    )]
    ConfigNotFound { path: PathBuf },

    /// Configuration parse error
    #[error("Failed to parse configuration: {path}")]
    #[diagnostic(code(zdm::config::parse_error))]
    ConfigParseError {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// Invalid configuration value
    #[error("Invalid configuration value for '{key}': {message}")]
    #[diagnostic(code(zdm::config::invalid_value))]
    ConfigInvalidValue { key: String, message: String },

    /// Git error
    #[error("Git error: {message}")]
    #[diagnostic(code(zdm::git::error))]
    GitError {
        message: String,
        #[source]
        source: Option<git2::Error>,
    },

    /// Git repository not found
    #[error("Not a git repository: {path}")]
    #[diagnostic(
        code(zdm::git::not_repo),
        help("The --diff flag requires a git repository")
    )]
    NotAGitRepository { path: PathBuf },

    /// Invalid git reference
    #[error("Invalid git reference: {reference}")]
    #[diagnostic(
        code(zdm::git::invalid_ref),
        help("Specify a valid branch, tag, or commit SHA")
    )]
    InvalidGitReference { reference: String },

    /// Unknown rule
    #[error("Unknown rule: {rule_id}")]
    #[diagnostic(
        code(zdm::rule::unknown),
        help("Run 'zdm --list-rules' to see available rules")
    )]
    UnknownRule { rule_id: String },

    /// Invalid path
    #[error("Invalid path: {path}")]
    #[diagnostic(code(zdm::io::invalid_path))]
    InvalidPath { path: PathBuf },

    /// Multiple errors collected
    #[error("Multiple errors occurred ({count} errors)")]
    #[diagnostic(code(zdm::multiple_errors))]
    Multiple { count: usize, errors: Vec<Error> },
}

impl Error {
    /// Create a file read error.
    pub fn file_read(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::FileRead {
            path: path.into(),
            source,
        }
    }

    /// Create a directory walk error.
    pub fn directory_walk(path: impl Into<PathBuf>, source: walkdir::Error) -> Self {
        Self::DirectoryWalk {
            path: path.into(),
            source,
        }
    }

    /// Create a file too large error.
    pub fn file_too_large(path: impl Into<PathBuf>, size: u64, max_size: u64) -> Self {
        Self::FileTooLarge {
            path: path.into(),
            size,
            max_size,
        }
    }

    /// Create a parse error.
    pub fn parse_error(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::ParseError {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Create a parse error with location.
    pub fn parse_error_with_location(path: impl Into<PathBuf>, line: usize, column: usize) -> Self {
        Self::ParseErrorWithLocation {
            path: path.into(),
            line,
            column,
        }
    }

    /// Create a config parse error.
    pub fn config_parse_error(path: impl Into<PathBuf>, source: toml::de::Error) -> Self {
        Self::ConfigParseError {
            path: path.into(),
            source,
        }
    }

    /// Create a git error.
    pub fn git_error(message: impl Into<String>, source: git2::Error) -> Self {
        Self::GitError {
            message: message.into(),
            source: Some(source),
        }
    }

    /// Create a git error without a source.
    pub fn git_error_msg(message: impl Into<String>) -> Self {
        Self::GitError {
            message: message.into(),
            source: None,
        }
    }

    /// Create an I/O error.
    pub fn io(source: std::io::Error, path: impl Into<PathBuf>) -> Self {
        Self::FileRead {
            path: path.into(),
            source,
        }
    }

    /// Create a parse error with a message.
    pub fn parse(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::ParseError {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Create an unknown rule error.
    pub fn unknown_rule(rule_id: impl Into<String>) -> Self {
        Self::UnknownRule {
            rule_id: rule_id.into(),
        }
    }

    /// Create a path not found error.
    pub fn path_not_found(path: impl Into<PathBuf>) -> Self {
        Self::InvalidPath { path: path.into() }
    }

    /// Returns true if this error is recoverable (we can continue processing other files).
    pub fn is_recoverable(&self) -> bool {
        match self {
            // File-level errors are recoverable - skip the file and continue
            Self::FileRead { .. } => true,
            Self::FileTooLarge { .. } => true,
            Self::ParseError { .. } => true,
            Self::ParseErrorWithLocation { .. } => true,
            Self::DirectoryWalk { .. } => true,

            // Config errors are not recoverable - we can't proceed without config
            Self::ConfigNotFound { .. } => false,
            Self::ConfigParseError { .. } => false,
            Self::ConfigInvalidValue { .. } => false,

            // Git errors are recoverable in non-diff mode
            Self::GitError { .. } => true,
            Self::NotAGitRepository { .. } => true,
            Self::InvalidGitReference { .. } => false,

            // Rule errors depend on context
            Self::UnknownRule { .. } => false,

            // Path errors are recoverable
            Self::InvalidPath { .. } => true,

            // Multiple errors - check if all are recoverable
            Self::Multiple { errors, .. } => errors.iter().all(|e| e.is_recoverable()),
        }
    }
}

/// Collector for accumulating errors during processing.
///
/// This allows us to continue processing files even when some fail,
/// collecting all errors for a final report.
#[derive(Debug, Default)]
pub struct ErrorCollector {
    errors: Vec<Error>,
}

impl ErrorCollector {
    /// Create a new error collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an error to the collector.
    pub fn push(&mut self, error: Error) {
        self.errors.push(error);
    }

    /// Returns true if any errors have been collected.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Returns the number of errors collected.
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Returns true if no errors have been collected.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// Convert into a single error (Multiple) or Ok if no errors.
    pub fn into_result(self) -> Result<()> {
        if self.errors.is_empty() {
            Ok(())
        } else if self.errors.len() == 1 {
            Err(self.errors.into_iter().next().unwrap())
        } else {
            Err(Error::Multiple {
                count: self.errors.len(),
                errors: self.errors,
            })
        }
    }

    /// Get an iterator over collected errors.
    pub fn iter(&self) -> impl Iterator<Item = &Error> {
        self.errors.iter()
    }

    /// Check if any non-recoverable errors have been collected.
    pub fn has_fatal_errors(&self) -> bool {
        self.errors.iter().any(|e| !e.is_recoverable())
    }
}
