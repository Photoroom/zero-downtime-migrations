//! Error types for zero-downtime-migrations.
//!
//! Covers file I/O, tree-sitter parsing, configuration loading, and the
//! git diff layer. Failures bubble through `Result<T>`; individual call
//! sites decide whether to abort (config parse) or skip-and-continue
//! (per-file parse — handled in `main.rs::run`).

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

    /// Configuration parse error
    #[error("Failed to parse configuration: {path}")]
    #[diagnostic(code(zdm::config::parse_error))]
    ConfigParseError {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// Git error
    #[error("Git error: {message}")]
    #[diagnostic(code(zdm::git::error))]
    GitError {
        message: String,
        #[source]
        source: Option<git2::Error>,
    },

    /// Invalid git reference. Currently has no producer; reserved for the
    /// upcoming refinement that distinguishes "ref does not exist" from
    /// generic git failures in `GitRepo::tree_at`.
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
        help("Run 'zdm rule <id>' to see documentation for a specific rule")
    )]
    UnknownRule { rule_id: String },

    /// Invalid path
    #[error("Invalid path: {path}")]
    #[diagnostic(code(zdm::io::invalid_path))]
    InvalidPath { path: PathBuf },
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

    /// Create a git error without a wrapped source.
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
}
