//! Django migration AST abstraction layer.
//!
//! This module provides typed Rust representations of Django migration
//! operations extracted from tree-sitter Python AST nodes.

pub(crate) mod extractor;
mod operations;

pub(crate) use extractor::MigrationExtractor;
pub(crate) use operations::{
    any_sql_statement, sql_statement_contains_concurrently, sql_statement_contains_create_index,
    sql_statement_contains_drop_index, sql_statement_contains_reindex,
};
pub use operations::{
    ConstraintOperation, ConstraintType, FieldInfo, FieldOperation, IndexOperation, ModelOperation,
    Operation, OperationData, OperationType, RunPythonOperation, RunSQLOperation,
    SeparateDatabaseAndStateOperation,
};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::slice;

use crate::diagnostics::Span;

/// A parsed Django migration file with extracted operations.
///
/// `#[non_exhaustive]` so future fields (new spans, new metadata)
/// are additive — out-of-tree code must destructure with `..`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Migration {
    /// The file path of the migration.
    pub path: PathBuf,
    /// Whether the migration has `atomic = False`.
    pub is_non_atomic: bool,
    /// The list of operations in this migration.
    pub operations: Vec<Operation>,
    /// Import statements that may be relevant for linting.
    pub imports: Vec<Import>,
    /// Span of the `class Migration(...)` definition, when present. Used
    /// as the anchor for class-level diagnostics (e.g. R004's missing
    /// `atomic = False`) so a `# zdm: ignore` on or above the class line
    /// suppresses the diagnostic correctly.
    pub class_span: Option<Span>,
    /// For each source line that carried a `# zdm: ignore RXXX[, RYYY]`
    /// comment, the set of rule IDs the user asked to suppress at that
    /// line. Lines are 1-indexed.
    pub line_ignores: BTreeMap<usize, BTreeSet<String>>,
}

impl Migration {
    /// Load and extract a migration from a file on disk: enforces the
    /// size cap, parses with tree-sitter, and returns the typed
    /// `Migration`. The recommended entry point for programmatic consumers.
    pub fn from_path(path: &std::path::Path) -> crate::error::Result<Self> {
        let parsed = crate::parser::ParsedMigration::parse_file(path)?;
        MigrationExtractor::new(&parsed)
            .extract(path)
            .map_err(|e| crate::error::Error::parse(path.to_path_buf(), e.to_string()))
    }

    /// Extract a migration from in-memory source. Same pipeline
    /// as [`Self::from_path`], minus the disk read — useful for
    /// linting staged-but-uncommitted content (where we read the
    /// git index blob) and for unit-testing custom rules with a
    /// string literal.
    pub fn from_source(path: &std::path::Path, source: &str) -> crate::error::Result<Self> {
        crate::parser::check_size(path, source.len() as u64)?;
        let parsed = crate::parser::ParsedMigration::parse(source)
            .map_err(|e| crate::error::Error::parse(path.to_path_buf(), e.to_string()))?;
        if parsed.has_errors() {
            return Err(crate::error::Error::parse(
                path.to_path_buf(),
                "syntax error in migration file".to_string(),
            ));
        }
        MigrationExtractor::new(&parsed)
            .extract(path)
            .map_err(|e| crate::error::Error::parse(path.to_path_buf(), e.to_string()))
    }

    /// Get top-level operations of a specific type.
    pub fn top_level_operations_of_type(
        &self,
        op_type: OperationType,
    ) -> impl Iterator<Item = &Operation> {
        self.operations
            .iter()
            .filter(move |op| op.op_type == op_type)
    }

    /// Get database-effective operations in execution order.
    ///
    /// Top-level `SeparateDatabaseAndState` wrappers are expanded in place to
    /// their literal `database_operations` arm. Wrapped operations retain their
    /// original spans. State-side operations are metadata-only and are
    /// deliberately omitted.
    pub fn database_effective_operations(&self) -> impl Iterator<Item = &Operation> {
        DatabaseEffectiveOperations::new(&self.operations)
    }

    /// Get database-effective operations of a specific type in execution order.
    pub fn database_effective_operations_of_type(
        &self,
        op_type: OperationType,
    ) -> impl Iterator<Item = &Operation> {
        self.database_effective_operations()
            .filter(move |op| op.op_type == op_type)
    }

    /// Whether the given rule ID is suppressed for a diagnostic whose
    /// span runs from `start_line` to `end_line` (inclusive). A
    /// `# zdm: ignore <id>` comment counts when it appears anywhere
    /// within that range, or on the line immediately preceding it.
    pub fn is_rule_suppressed_at(&self, rule_id: &str, start_line: usize, end_line: usize) -> bool {
        // Also honour an ignore on the line immediately above the span.
        // `start_line` is 1-indexed; clamp to 1 so we never probe line 0.
        let lookup_start = start_line.saturating_sub(1).max(1);
        (lookup_start..=end_line).any(|line| {
            self.line_ignores
                .get(&line)
                .is_some_and(|set| set.contains(rule_id))
        })
    }
}

/// Stack-based DFS iterator that expands `SeparateDatabaseAndState`
/// wrappers in place while preserving each operation's original span.
struct DatabaseEffectiveOperations<'a> {
    stack: Vec<slice::Iter<'a, Operation>>,
}

impl<'a> DatabaseEffectiveOperations<'a> {
    fn new(operations: &'a [Operation]) -> Self {
        Self {
            stack: vec![operations.iter()],
        }
    }
}

impl<'a> Iterator for DatabaseEffectiveOperations<'a> {
    type Item = &'a Operation;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let iter = self.stack.last_mut()?;
            let Some(op) = iter.next() else {
                self.stack.pop();
                continue;
            };

            if let OperationData::SeparateDatabaseAndState(data) = &op.data {
                self.stack.push(data.database_operations.iter());
                continue;
            }

            return Some(op);
        }
    }
}

/// An import statement in the migration file.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Import {
    /// For `from X import ...`, the module path `X`. `None` for plain
    /// `import X` statements (the matcher `is_direct_model_import`
    /// short-circuits on `None`).
    pub module: Option<String>,
    /// For `from X import a, b, c`, the imported names. Aliases are
    /// recorded under the original name (`from X import a as b` → `"a"`).
    pub names: Vec<String>,
    /// The span of the import statement.
    pub span: Span,
}

impl Import {
    /// Check whether this import looks like it brings a Django *model class*
    /// into scope (the thing R014 wants to flag). Imports from
    /// `django.db.models` or `django.contrib.*` are skipped because those
    /// give you the field/utility classes, not historical model state. A
    /// `from .models import some_helper` for a snake_case helper is also
    /// not flagged — only PascalCase names (the convention for model
    /// classes) trigger. Wildcard imports (`from .models import *`) are
    /// flagged because we can't see which names land in scope.
    pub fn is_direct_model_import(&self) -> bool {
        let Some(module) = &self.module else {
            return false;
        };
        if !module_path_ends_with_models(module) {
            return false;
        }
        if module.starts_with("django.") {
            return false;
        }
        self.names
            .iter()
            .any(|n| n == "*" || starts_with_uppercase(n))
    }
}

/// Returns true if `module` ends with a `models` segment, e.g.
/// `myapp.models`, `package.models`, or just `models`. Relative imports
/// like `.models` and `..models` also count.
fn module_path_ends_with_models(module: &str) -> bool {
    let trimmed = module.trim_start_matches('.');
    trimmed == "models" || trimmed.ends_with(".models")
}

fn starts_with_uppercase(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}
