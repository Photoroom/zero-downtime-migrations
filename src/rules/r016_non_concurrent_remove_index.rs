//! R016: Non-concurrent RemoveIndex
//!
//! Detects uses of `migrations.RemoveIndex` instead of `RemoveIndexConcurrently`.
//! Regular `RemoveIndex` takes an exclusive lock on the table.

use crate::ast::{Migration, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects non-concurrent RemoveIndex operations.
pub struct R016NonConcurrentRemoveIndex;

impl Rule for R016NonConcurrentRemoveIndex {
    fn id(&self) -> &'static str {
        "R016"
    }

    fn name(&self) -> &'static str {
        "non-concurrent-remove-index"
    }

    fn description(&self) -> &'static str {
        "RemoveIndex takes an exclusive lock on the table. Use RemoveIndexConcurrently \
         instead to drop the index without blocking reads and writes."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::RemoveIndex) {
            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: "Use RemoveIndexConcurrently instead of RemoveIndex to avoid table locks"
                    .to_string(),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: op.span,
                help: Some(
                    "Replace migrations.RemoveIndex with RemoveIndexConcurrently from \
                     django.contrib.postgres.operations"
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

    const REMOVE_INDEX_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RemoveIndex(
            model_name='product',
            name='product_name_idx',
        ),
    ]
"#;

    const REMOVE_INDEX_CONCURRENT_GOOD: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import RemoveIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        RemoveIndexConcurrently(
            model_name='product',
            name='product_name_idx',
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
        R016NonConcurrentRemoveIndex.check(&migration, &ctx)
    }

    #[test]
    fn test_remove_index_bad() {
        let diagnostics = check_migration(REMOVE_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R016");
    }

    #[test]
    fn test_remove_index_concurrent_good() {
        let diagnostics = check_migration(REMOVE_INDEX_CONCURRENT_GOOD);
        assert!(diagnostics.is_empty());
    }
}
