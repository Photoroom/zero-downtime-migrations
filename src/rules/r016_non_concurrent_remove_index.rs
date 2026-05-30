//! R016: Non-concurrent RemoveIndex
//!
//! Detects uses of `migrations.RemoveIndex` instead of
//! `RemoveIndexConcurrently`. Regular `RemoveIndex` takes an
//! ACCESS EXCLUSIVE lock on the table for the duration of the drop,
//! which blocks reads and writes — fine on an empty table but a
//! real outage on a live one.

use crate::ast::{Migration, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{walk_with_created_models, Rule, RuleContext};

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
        walk_with_created_models(migration, |op, created| {
            if op.op_type != OperationType::RemoveIndex {
                return;
            }
            if op.model_name().is_some_and(|m| created.contains(m)) {
                return;
            }

            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    "Use RemoveIndexConcurrently instead of RemoveIndex to avoid table locks",
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(
                    "Replace migrations.RemoveIndex with RemoveIndexConcurrently from \
                     django.contrib.postgres.operations. The concurrent form takes \
                     SHARE UPDATE EXCLUSIVE instead of ACCESS EXCLUSIVE and must run \
                     outside a transaction (`atomic = False`).",
                ),
            );
        });

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        crate::rules::test_support::check_rule(&R016NonConcurrentRemoveIndex, source)
    }

    #[test]
    fn test_remove_index_bad() {
        let diagnostics = check_migration(REMOVE_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R016");
    }

    const REMOVE_INDEX_POSITIONAL_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RemoveIndex('product', 'product_name_idx'),
    ]
"#;

    #[test]
    fn test_remove_index_positional_bad() {
        let diagnostics = check_migration(REMOVE_INDEX_POSITIONAL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R016");
    }

    #[test]
    fn test_remove_index_concurrent_good() {
        let diagnostics = check_migration(REMOVE_INDEX_CONCURRENT_GOOD);
        assert!(diagnostics.is_empty());
    }

    const REMOVE_INDEX_ON_FRESH_MODEL_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
                ('name', models.CharField(max_length=255)),
            ],
            options={'indexes': [models.Index(fields=['name'], name='product_name_idx')]},
        ),
        migrations.RemoveIndex(
            model_name='product',
            name='product_name_idx',
        ),
    ]
"#;

    #[test]
    fn test_remove_index_on_fresh_model_is_exempt() {
        // A model created in this same migration has no live traffic
        // when the migration runs, so the brief ACCESS EXCLUSIVE lock
        // from a plain RemoveIndex is harmless. Mirrors R001.
        let diagnostics = check_migration(REMOVE_INDEX_ON_FRESH_MODEL_GOOD);
        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:?}",
        );
    }

    const POSITIONAL_REMOVE_INDEX_ON_FRESH_MODEL_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
                ('name', models.CharField(max_length=255)),
            ],
            options={'indexes': [models.Index(fields=['name'], name='product_name_idx')]},
        ),
        migrations.RemoveIndex('product', 'product_name_idx'),
    ]
"#;

    #[test]
    fn test_positional_remove_index_on_fresh_model_is_exempt() {
        let diagnostics = check_migration(POSITIONAL_REMOVE_INDEX_ON_FRESH_MODEL_GOOD);
        assert!(
            diagnostics.is_empty(),
            "positional RemoveIndex on a fresh model should not fire R016, got: {diagnostics:?}",
        );
    }

    const REMOVE_INDEX_ON_FRESH_AND_EXISTING_MODELS: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[('id', models.BigAutoField(primary_key=True))],
        ),
        migrations.RemoveIndex(model_name='product', name='product_legacy_idx'),
        migrations.RemoveIndex(model_name='order', name='order_legacy_idx'),
    ]
"#;

    #[test]
    fn test_remove_index_on_existing_model_still_flagged_when_fresh_model_present() {
        // The exemption is per-operation: a fresh-model RemoveIndex is
        // exempt, but a sibling RemoveIndex on an unrelated existing
        // model must still fire.
        let diagnostics = check_migration(REMOVE_INDEX_ON_FRESH_AND_EXISTING_MODELS);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R016");
    }

    const REMOVE_INDEX_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.RemoveIndex(model_name='product', name='product_legacy_idx'),
        migrations.CreateModel(
            name='Product',
            fields=[('id', models.BigAutoField(primary_key=True))],
        ),
    ]
"#;

    #[test]
    fn test_remove_index_before_createmodel_is_not_exempted() {
        // Order-aware exemption: a CreateModel that runs *after* the
        // RemoveIndex cannot retroactively make the RemoveIndex safe.
        // An order-blind `is_model_created` lookup would silently
        // exempt this; the source-order walk correctly flags it.
        let diagnostics = check_migration(REMOVE_INDEX_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R016");
    }

    const WRAPPED_REMOVE_INDEX_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RemoveIndex(
                    model_name='product',
                    name='product_name_idx',
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_database_remove_index_is_flagged() {
        let diagnostics = check_migration(WRAPPED_REMOVE_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R016");
    }
}
