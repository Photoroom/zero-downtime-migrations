//! R004: Missing atomic=False for concurrent operations
//!
//! Concurrent index operations (AddIndexConcurrently, RemoveIndexConcurrently)
//! cannot run inside a transaction. The migration must have `atomic = False`.

use crate::ast::Migration;
use crate::diagnostics::{Diagnostic, Severity, Span};
use crate::rules::{Rule, RuleContext};

/// Rule that detects concurrent operations without atomic=False.
pub struct R004MissingAtomicFalse;

impl Rule for R004MissingAtomicFalse {
    fn id(&self) -> &'static str {
        "R004"
    }

    fn name(&self) -> &'static str {
        "missing-atomic-false"
    }

    fn description(&self) -> &'static str {
        "Concurrent index operations cannot run inside a transaction. \
         Add `atomic = False` to the Migration class."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Check if migration has any concurrent operations
        let has_concurrent = migration
            .operations
            .iter()
            .any(|op| op.op_type.is_concurrent());

        if has_concurrent && !migration.is_non_atomic {
            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: "Migration uses concurrent operations but is not marked as non-atomic"
                    .to_string(),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: Span::default(), // Class-level issue
                help: Some(
                    "Add `atomic = False` to the Migration class to allow concurrent operations"
                        .to_string(),
                ),
                fix: None,
            });
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::extractor::MigrationExtractor;
    use crate::config::Config;
    use crate::parser::ParsedMigration;
    use std::path::Path;

    const CONCURRENT_NO_ATOMIC_BAD: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):

    operations = [
        AddIndexConcurrently(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    const CONCURRENT_WITH_ATOMIC_GOOD: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    const NON_CONCURRENT_NO_ATOMIC_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[],
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("test.py"),
        };
        R004MissingAtomicFalse.check(&migration, &ctx)
    }

    #[test]
    fn test_concurrent_no_atomic_bad() {
        let diagnostics = check_migration(CONCURRENT_NO_ATOMIC_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R004");
    }

    #[test]
    fn test_concurrent_with_atomic_good() {
        let diagnostics = check_migration(CONCURRENT_WITH_ATOMIC_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_non_concurrent_no_atomic_good() {
        let diagnostics = check_migration(NON_CONCURRENT_NO_ATOMIC_GOOD);
        assert!(diagnostics.is_empty());
    }
}
