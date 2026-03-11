//! Django migration AST abstraction layer.
//!
//! This module provides typed Rust representations of Django migration
//! operations extracted from tree-sitter Python AST nodes.

pub mod extractor;
mod operations;

pub use extractor::MigrationExtractor;
pub use operations::*;

use std::path::PathBuf;

use crate::diagnostics::Span;

/// A parsed Django migration file with extracted operations.
#[derive(Debug, Clone)]
pub struct Migration {
    /// The file path of the migration.
    pub path: PathBuf,
    /// Whether the migration has `atomic = False`.
    pub is_non_atomic: bool,
    /// The list of operations in this migration.
    pub operations: Vec<Operation>,
    /// Import statements that may be relevant for linting.
    pub imports: Vec<Import>,
    /// Model names created in this migration (for exemption tracking).
    pub created_models: Vec<String>,
}

impl Migration {
    /// Get all operations of a specific type.
    pub fn operations_of_type(&self, op_type: OperationType) -> impl Iterator<Item = &Operation> {
        self.operations
            .iter()
            .filter(move |op| op.op_type == op_type)
    }

    /// Check if a model was created in this migration.
    pub fn is_model_created(&self, model_name: &str) -> bool {
        self.created_models
            .iter()
            .any(|name| name.eq_ignore_ascii_case(model_name))
    }
}

/// An import statement in the migration file.
#[derive(Debug, Clone)]
pub struct Import {
    /// The full import text.
    pub text: String,
    /// The span of the import statement.
    pub span: Span,
}

impl Import {
    /// Check if this is a concurrent index operation import.
    pub fn is_concurrent_index_import(&self) -> bool {
        self.text.contains("AddIndexConcurrently") || self.text.contains("RemoveIndexConcurrently")
    }

    /// Check if this is a direct model import (bad practice).
    pub fn is_direct_model_import(&self) -> bool {
        self.text.contains(".models import") && !self.text.contains("django.db.models")
    }
}
