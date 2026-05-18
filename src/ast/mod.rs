//! Django migration AST abstraction layer.
//!
//! This module provides typed Rust representations of Django migration
//! operations extracted from tree-sitter Python AST nodes.

pub mod extractor;
mod operations;

pub use extractor::MigrationExtractor;
pub(crate) use operations::strip_sql_noise;
pub use operations::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

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
    /// Operations extracted from
    /// `SeparateDatabaseAndState(database_operations=[...])` arms in this
    /// migration. These represent the "database-side" half of a two-step
    /// deployment — they execute real schema changes without the
    /// state-side preamble, so a rule that scans for schema-locking
    /// patterns can inspect this field alongside `operations` to avoid
    /// silently ignoring locks hidden inside an SDaS wrapper.
    ///
    /// The rule traversal treats these as database-effective operations
    /// without inheriting top-level `CreateModel` exemptions, because
    /// the wrapper implies the operation targets a live schema.
    ///
    /// State-side operations are deliberately not surfaced because
    /// they're metadata-only — Django updates its migration state
    /// graph but doesn't touch the database. A rule that wants to
    /// inspect `state_operations` must walk the original
    /// `OperationData::SeparateDatabaseAndState` payload itself.
    pub wrapped_database_ops: Vec<Operation>,
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
    /// Get all operations of a specific type.
    pub fn operations_of_type(&self, op_type: OperationType) -> impl Iterator<Item = &Operation> {
        self.operations
            .iter()
            .filter(move |op| op.op_type == op_type)
    }

    /// Whether the given rule ID is suppressed for a diagnostic whose
    /// span runs from `start_line` to `end_line` (inclusive). A
    /// `# zdm: ignore <id>` comment counts when it appears anywhere
    /// within that range, or on the line immediately preceding it.
    pub fn is_rule_suppressed_at(&self, rule_id: &str, start_line: usize, end_line: usize) -> bool {
        // `start_line` is 1-indexed (tree-sitter rows + 1), so the
        // saturating_sub clamps to 1 on the off chance a default span
        // produces `start_line == 0`.
        let lookup_start = start_line.saturating_sub(1).max(1);
        (lookup_start..=end_line).any(|line| {
            self.line_ignores
                .get(&line)
                .is_some_and(|set| set.contains(rule_id))
        })
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
