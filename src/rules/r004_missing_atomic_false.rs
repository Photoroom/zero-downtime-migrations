//! R004: Missing atomic=False for concurrent operations
//!
//! Concurrent index operations (AddIndexConcurrently, RemoveIndexConcurrently)
//! cannot run inside a transaction. The migration must have `atomic = False`.

use crate::ast::Migration;
use crate::diagnostics::{Diagnostic, Severity};
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
            // Anchor the diagnostic at the `class Migration(...)` line.
            // Falling back to `Span::default()` (line 1) would make
            // inline suppression with `# zdm: ignore R004` only work at
            // the top of the file, which is rarely where users write it.
            let span = migration.class_span.unwrap_or_default();
            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: "Migration uses concurrent operations but is not marked as non-atomic"
                    .to_string(),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span,
                help: Some(
                    "Add `atomic = False` to the Migration class to allow concurrent operations"
                        .to_string(),
                ),
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

    #[test]
    fn test_diagnostic_anchors_at_migration_class_line() {
        // Previously R004 used `Span::default()` (line 1), so a
        // `# zdm: ignore R004` placed on or above the Migration class
        // line couldn't suppress the diagnostic — only line 1 worked.
        // Anchor the span at the class definition instead.
        let parsed = ParsedMigration::parse(CONCURRENT_NO_ATOMIC_BAD).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("test.py"),
        };
        let diagnostics = R004MissingAtomicFalse.check(&migration, &ctx);

        assert_eq!(diagnostics.len(), 1);
        // The fixture's `class Migration` starts on line 6 of the source
        // (the raw string starts with a leading newline, then three import
        // lines, two blank lines, then the class).
        assert_eq!(diagnostics[0].span.start_line, 6);
    }

    const SUPPRESSED_ABOVE_CLASS: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


# zdm: ignore R004
class Migration(migrations.Migration):

    operations = [
        AddIndexConcurrently(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn test_inline_ignore_above_class_suppresses_r004() {
        // End-to-end check: the diagnostic span now anchors at the
        // class, so a `# zdm: ignore R004` on the line above the class
        // falls within the suppression lookup window.
        let parsed = ParsedMigration::parse(SUPPRESSED_ABOVE_CLASS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        // Apply the registry's retain step (which is where suppression
        // actually fires); we reproduce it here in miniature so the
        // assertion is unambiguous.
        let raw = R004MissingAtomicFalse.check(
            &migration,
            &RuleContext {
                config: &Config::default(),
                path: Path::new("test.py"),
            },
        );
        let surviving: Vec<_> = raw
            .into_iter()
            .filter(|d| {
                !migration.is_rule_suppressed_at(d.rule_id, d.span.start_line, d.span.end_line)
            })
            .collect();
        assert!(surviving.is_empty());
    }
}
