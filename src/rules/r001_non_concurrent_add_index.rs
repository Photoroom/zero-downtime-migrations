//! R001: Non-concurrent AddIndex
//!
//! Detects uses of `migrations.AddIndex` instead of `AddIndexConcurrently`.
//! Regular `AddIndex` takes an exclusive lock on the table, blocking all reads
//! and writes until the index is built.

use crate::ast::{Migration, ModelOperation, Operation, OperationData, OperationType};
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

        // Top-level walk in source order so the CreateModel
        // exemption only honours models created *before* the
        // AddIndex. The order-blind `is_model_created` would
        // silently exempt an AddIndex placed above its
        // CreateModel — the same false negative R002, R006, R016,
        // and R017 fixed in this PR.
        let mut created_so_far: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for op in &migration.operations {
            if op.op_type == OperationType::CreateModel {
                if let OperationData::Model(ModelOperation { name, .. }) = &op.data {
                    created_so_far.insert(name.to_lowercase());
                }
                continue;
            }
            if op.op_type != OperationType::AddIndex {
                continue;
            }
            if let OperationData::Index(index_op) = &op.data {
                if created_so_far.contains(&index_op.model_name.to_lowercase()) {
                    continue;
                }
            }
            diagnostics.push(self.diagnose(op, ctx));
        }

        // AddIndex wrapped inside `SeparateDatabaseAndState(
        // database_operations=[...])`. Django runs the wrapped op
        // against the live schema, so the same ACCESS EXCLUSIVE lock
        // applies — wrapping doesn't make a non-concurrent index any
        // safer. The CreateModel exemption isn't applied here: the
        // table the wrapped op targets is, by definition, already
        // live (otherwise the wrapping wouldn't be necessary).
        for op in migration
            .wrapped_database_ops
            .iter()
            .filter(|op| op.op_type == OperationType::AddIndex)
        {
            diagnostics.push(self.diagnose(op, ctx));
        }

        diagnostics
    }
}

impl R001NonConcurrentAddIndex {
    fn diagnose(&self, op: &Operation, ctx: &RuleContext) -> Diagnostic {
        Diagnostic {
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
        }
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

    const ADD_INDEX_INSIDE_SDAS_DATABASE_OPS_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.AddIndex(
                    model_name='product',
                    index=models.Index(fields=['name'], name='product_name_idx'),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_add_index_inside_sdas_database_ops_is_flagged() {
        // `SeparateDatabaseAndState(database_operations=[AddIndex(...)])`
        // still runs a non-concurrent CREATE INDEX against the live
        // schema. Wrapping the op in SDaS doesn't make the lock
        // safer — it just hides the operation from a naive top-level
        // walk. R001 now consumes `wrapped_database_ops` so the
        // hidden lock surfaces.
        let diagnostics = check_migration(ADD_INDEX_INSIDE_SDAS_DATABASE_OPS_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R001");
    }

    const ADD_INDEX_INSIDE_SDAS_STATE_OPS_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            state_operations=[
                migrations.AddIndex(
                    model_name='product',
                    index=models.Index(fields=['name'], name='product_name_idx'),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_add_index_inside_sdas_state_ops_is_not_flagged() {
        // `state_operations` is metadata-only — Django updates its
        // migration state graph but does not touch the database.
        // The extractor deliberately omits state-side ops from
        // `wrapped_database_ops`, so R001 leaves them alone.
        let diagnostics = check_migration(ADD_INDEX_INSIDE_SDAS_STATE_OPS_GOOD);
        assert!(diagnostics.is_empty());
    }

    const ADDINDEX_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('name', models.CharField(max_length=255)),
            ],
        ),
    ]
"#;

    #[test]
    fn test_addindex_before_createmodel_is_not_exempted() {
        // Order-aware exemption: a CreateModel that runs *after*
        // the AddIndex cannot retroactively make the AddIndex safe.
        // R001 was the last rule still using the order-blind
        // `is_model_created` lookup; this test pins the fix that
        // brings it in line with R002/R006/R010/R016/R017.
        let diagnostics = check_migration(ADDINDEX_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R001");
    }

    const CREATEMODEL_BEFORE_ADDINDEX_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('name', models.CharField(max_length=255)),
            ],
        ),
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn test_createmodel_before_addindex_is_exempted() {
        // The "exempt-something" pair for the order-aware fix
        // above. An `exempt nothing` regression in `created_so_far`
        // would otherwise only trip `test_add_index_bad` (which has
        // no CreateModel at all); this pair makes the exemption
        // path explicit and symmetric with the no-exempt pair.
        let diagnostics = check_migration(CREATEMODEL_BEFORE_ADDINDEX_GOOD);
        assert!(
            diagnostics.is_empty(),
            "AddIndex on a model created earlier in the same migration should not fire R001, got: {diagnostics:?}",
        );
    }
}
