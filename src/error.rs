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
///
/// `#[non_exhaustive]` so downstream code that exhaustively
/// matches on the variants doesn't break when we add a new error
/// path. The `UnknownRule` variant also gained an `available`
/// field during this PR's review pass — pre-existing callers of
/// `Error::unknown_rule` were updated in lockstep, but
/// out-of-tree callers constructing the variant by struct-literal
/// would have broken silently before this attribute landed.
#[derive(Error, Debug, Diagnostic)]
#[non_exhaustive]
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

    /// Configuration parse error.
    ///
    /// The `source` is an opaque [`ConfigParseSource`] so that a
    /// future major version of the underlying TOML library can be
    /// adopted without it being a breaking change for our consumers.
    #[error("Failed to parse configuration: {path}")]
    #[diagnostic(code(zdm::config::parse_error))]
    ConfigParseError {
        path: PathBuf,
        #[source]
        source: ConfigParseSource,
    },

    /// Git error.
    ///
    /// The `source` is an opaque [`GitSource`] for the same
    /// reason as `ConfigParseError`: keeps git2 out of our
    /// public ABI.
    #[error("Git error: {message}")]
    #[diagnostic(code(zdm::git::error))]
    GitError {
        message: String,
        #[source]
        source: Option<GitSource>,
    },

    /// The supplied `--diff` reference does not exist (libgit2's NotFound).
    /// Distinguished from generic git failures so the CLI can suggest a
    /// likely fix (e.g. fetching `origin/main` after a shallow clone).
    #[error("Invalid git reference: {reference}")]
    #[diagnostic(
        code(zdm::git::invalid_ref),
        help(
            "Specify a valid branch, tag, or commit SHA. \
             For `origin/*` refs, you may need `git fetch origin` first — \
             shallow clones and fresh checkouts omit remote-tracking refs."
        )
    )]
    InvalidGitReference { reference: String },

    /// Invalid command-line flag or argument combination.
    #[error("CLI usage error: {message}")]
    #[diagnostic(code(zdm::cli::usage_error))]
    CliUsage { message: String },

    /// Unknown rule. Format is single-line because the top-level
    /// error sink runs the chain through `sanitize_single_line`,
    /// which would otherwise escape an embedded `\n` into `\x0a`.
    #[error("Unknown rule: {rule_id} (available rules: {})", available.join(", "))]
    #[diagnostic(
        code(zdm::rule::unknown),
        help("Run 'zdm rule <id>' to see documentation for a specific rule")
    )]
    UnknownRule {
        rule_id: String,
        /// All rule IDs the binary knows about, sorted, for the
        /// "available rules: …" line in the error message. Populated
        /// at the error-construction site (the CLI dispatcher) so the
        /// error type itself doesn't depend on the rule registries.
        available: Vec<String>,
    },

    /// Invalid path
    #[error("Invalid path: {path}")]
    #[diagnostic(code(zdm::io::invalid_path))]
    InvalidPath { path: PathBuf },

    /// A glob pattern from `exclude` or `allowed-file-patterns` could
    /// not be compiled. Surfaced at config-load time so misconfigured
    /// patterns fail loudly rather than being silently ignored at the
    /// match site.
    #[error("Invalid glob pattern '{pattern}': {message}")]
    #[diagnostic(
        code(zdm::config::invalid_glob),
        help(
            "Glob patterns use the `glob` crate's syntax: `**` matches any number \
             of path segments, `*` matches any characters except `/`"
        )
    )]
    InvalidGlobPattern { pattern: String, message: String },
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
            source: source.into(),
        }
    }

    /// Create a git error without a wrapped source.
    pub fn git_error_msg(message: impl Into<String>) -> Self {
        Self::GitError {
            message: message.into(),
            source: None,
        }
    }

    /// Create a CLI usage error.
    pub fn cli_usage(message: impl Into<String>) -> Self {
        Self::CliUsage {
            message: message.into(),
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

    /// Create an unknown rule error. `available` should be every rule
    /// ID the binary recognises (per-file + changeset registries),
    /// sorted, so the rendered error can suggest valid alternatives.
    pub fn unknown_rule(rule_id: impl Into<String>, available: Vec<String>) -> Self {
        Self::UnknownRule {
            rule_id: rule_id.into(),
            available,
        }
    }

    /// Create a path not found error.
    pub fn path_not_found(path: impl Into<PathBuf>) -> Self {
        Self::InvalidPath { path: path.into() }
    }
}

/// Opaque wrapper around a TOML parse failure. Implements
/// `std::error::Error + Display` so it shows up as the
/// `#[source]` of [`Error::ConfigParseError`]; consumers can
/// print it but cannot downcast to the underlying TOML library
/// type. That keeps the TOML crate out of our public ABI — a
/// future major version of it is not a breaking change for us.
#[derive(Debug)]
pub struct ConfigParseSource(toml::de::Error);

impl std::fmt::Display for ConfigParseSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for ConfigParseSource {}

impl From<toml::de::Error> for ConfigParseSource {
    fn from(e: toml::de::Error) -> Self {
        Self(e)
    }
}

/// Opaque wrapper around a libgit2 failure. See
/// [`ConfigParseSource`] for the rationale.
#[derive(Debug)]
pub struct GitSource(git2::Error);

impl std::fmt::Display for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for GitSource {}

impl From<git2::Error> for GitSource {
    fn from(e: git2::Error) -> Self {
        Self(e)
    }
}
