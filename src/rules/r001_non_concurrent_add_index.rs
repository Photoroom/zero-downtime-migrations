//! R001: Non-concurrent AddIndex
//!
//! Detects uses of `migrations.AddIndex` instead of `AddIndexConcurrently`.
//! Regular `AddIndex` takes an exclusive lock on the table, blocking all reads
//! and writes until the index is built.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects non-concurrent AddIndex operations.
pub struct R001NonConcurrentAddIndex;

impl Rule for R001NonConcurrentAddIndex {
    fn id(&self) -> &'static str {
        "R001"
    }

    fn name(&self) -> &'static str {
        "non-concurrent-add-index"
    }

    fn description(&self) -> &'static str {
        "AddIndex takes an exclusive lock on the table. Use AddIndexConcurrently instead \
         to build the index without blocking reads and writes."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::AddIndex) {
            // Check for CreateModel exemption
            if let OperationData::Index(index_op) = &op.data {
                if migration.is_model_created(&index_op.model_name) {
                    continue; // Skip - model was created in same migration
                }
            }

            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: "Use AddIndexConcurrently instead of AddIndex to avoid table locks"
                    .to_string(),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: op.span,
                help: Some(
                    "Replace migrations.AddIndex with AddIndexConcurrently from \
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

    const ADD_INDEX_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    const ADD_INDEX_CONCURRENT_GOOD: &str = r#"
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

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("test.py"),
        };
        R001NonConcurrentAddIndex.check(&migration, &ctx)
    }

    #[test]
    fn test_add_index_bad() {
        let diagnostics = check_migration(ADD_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R001");
        assert_eq!(diagnostics[0].severity, Severity::Error);
    }

    #[test]
    fn test_add_index_concurrent_good() {
        let diagnostics = check_migration(ADD_INDEX_CONCURRENT_GOOD);
        assert!(diagnostics.is_empty());
    }
}
