//! R006: AddField with ForeignKey on an existing model.
//!
//! Adding a ForeignKey via `AddField` against an existing table is doubly
//! expensive: it builds the index implicitly (acquiring a lock for the
//! duration) and validates the constraint against every existing row.
//!
//! This rule was originally split across R006 (FK locks the table) and
//! R007 (FK should be preceded by a concurrent index). They fired on the
//! same operation with overlapping prescriptions; R007 was retired and
//! R006 now uses the stricter policy: a prebuilt index does not make the
//! `AddField(ForeignKey)` operation itself safe.

use crate::ast::{FieldType, Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{walk_with_created_models, Rule, RuleContext};

/// Rule that detects AddField with ForeignKey on an existing model.
pub struct R006AddFieldForeignKey;

impl Rule for R006AddFieldForeignKey {
    fn id(&self) -> &'static str {
        "R006"
    }

    fn name(&self) -> &'static str {
        "add-field-foreign-key"
    }

    fn description(&self) -> &'static str {
        "AddField with a ForeignKey on an existing table creates an implicit \
         index (locking the table while it builds) and validates the FK \
         constraint against every existing row. Split the work via \
         SeparateDatabaseAndState."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        walk_with_created_models(migration, |op, created| {
            if op.op_type != OperationType::AddField {
                return;
            }
            let OperationData::Field(data) = &op.data else {
                return;
            };
            let Some(field) = &data.field else { return };
            if !matches!(
                field.field_type,
                FieldType::ForeignKey | FieldType::OneToOneField
            ) {
                return;
            }

            if created.contains(&data.model_name) {
                return;
            }

            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    format!(
                        "AddField with ForeignKey on existing model '{}'",
                        data.model_name
                    ),
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(include_str!("help/r006_add_fk.txt")),
            );
        });

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADD_FK_EXISTING_MODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='order',
            name='product',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.product'),
        ),
    ]
"#;

    const ADD_FK_NEW_MODEL_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Order',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
            ],
        ),
        migrations.AddField(
            model_name='order',
            name='product',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.product'),
        ),
    ]
"#;

    const ADD_NON_FK_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='description',
            field=models.TextField(null=True),
        ),
    ]
"#;

    const ADD_FK_AFTER_CONCURRENT_INDEX_GOOD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['customer'], name='order_customer_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    const ADD_FK_BEFORE_CONCURRENT_INDEX_BAD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['customer'], name='order_customer_idx'),
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R006AddFieldForeignKey, source)
    }

    #[test]
    fn test_add_fk_existing_model_bad() {
        let diagnostics = check_migration(ADD_FK_EXISTING_MODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
        assert_eq!(diagnostics[0].severity, Severity::Error);
    }

    #[test]
    fn test_add_fk_new_model_good() {
        // FK on newly created model is exempt.
        let diagnostics = check_migration(ADD_FK_NEW_MODEL_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_add_non_fk_good() {
        let diagnostics = check_migration(ADD_NON_FK_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_add_fk_after_concurrent_index_still_flags() {
        let diagnostics = check_migration(ADD_FK_AFTER_CONCURRENT_INDEX_GOOD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    #[test]
    fn test_add_fk_before_concurrent_index_bad() {
        // Pre-creating the concurrent index AFTER the AddField does not
        // protect against the lock; R006 must still fire.
        let diagnostics = check_migration(ADD_FK_BEFORE_CONCURRENT_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const ADD_FK_AFTER_UNRELATED_CONCURRENT_INDEX_BAD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['status'], name='order_status_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    #[test]
    fn test_add_fk_after_unrelated_concurrent_index_bad() {
        // The previous heuristic exempted any FK on a model that had
        // *any* prior concurrent index, even when the index column
        // had nothing to do with the FK. R006 now requires the index
        // to cover the FK column.
        let diagnostics = check_migration(ADD_FK_AFTER_UNRELATED_CONCURRENT_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const ADD_FK_AFTER_CONCURRENT_INDEX_ON_FK_ID_COLUMN_GOOD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['customer_id'], name='order_customer_id_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    #[test]
    fn test_add_fk_after_concurrent_index_on_fk_id_column_still_flags() {
        // A prebuilt index can help later query plans, but the
        // AddField(ForeignKey) operation still validates the FK and
        // can create implicit index/constraint work. R006 keeps the
        // warning and points users to an explicit split.
        let diagnostics = check_migration(ADD_FK_AFTER_CONCURRENT_INDEX_ON_FK_ID_COLUMN_GOOD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const ADD_FK_AFTER_DESCENDING_CONCURRENT_INDEX_GOOD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['-customer'], name='order_customer_desc_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    #[test]
    fn test_add_fk_after_descending_concurrent_index_still_flags() {
        let diagnostics = check_migration(ADD_FK_AFTER_DESCENDING_CONCURRENT_INDEX_GOOD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const ADD_FK_AFTER_MULTI_COLUMN_INDEX_GOOD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(
                fields=['customer', 'status'],
                name='order_customer_status_idx',
            ),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    #[test]
    fn test_add_fk_after_multi_column_index_leading_with_fk_still_flags() {
        // Even a covering composite index does not make this one-step
        // AddField(ForeignKey) rollout the pattern R006 wants users to
        // ship. Keep the diagnostic conservative.
        let diagnostics = check_migration(ADD_FK_AFTER_MULTI_COLUMN_INDEX_GOOD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const ADD_FK_AFTER_MULTI_COLUMN_INDEX_TRAILING_FK_BAD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(
                fields=['status', 'customer'],
                name='order_status_customer_idx',
            ),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    #[test]
    fn test_add_fk_after_multi_column_index_trailing_with_fk_bad() {
        // Postgres won't use `(status, customer)` for an FK lookup on
        // `customer` — no usable leading prefix. The previous
        // set-membership check would have exempted this; the
        // leading-column check correctly flags it.
        let diagnostics = check_migration(ADD_FK_AFTER_MULTI_COLUMN_INDEX_TRAILING_FK_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const TWO_FKS_ONLY_ONE_COVERED: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['customer'], name='order_customer_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
        migrations.AddField(
            model_name='order',
            name='product',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.product'),
        ),
    ]
"#;

    #[test]
    fn test_two_fks_only_one_covered_flags_uncovered() {
        // Prebuilt indexes do not exempt either FK; both one-step
        // AddField(ForeignKey) operations should be reported.
        let diagnostics = check_migration(TWO_FKS_ONLY_ONE_COVERED);
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics.iter().all(|d| d.rule_id == "R006"));
    }

    const ADD_FK_WITH_DB_COLUMN_KNOWN_LIMITATION: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['legacy_customer'], name='order_legacy_customer_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(
                on_delete=models.CASCADE, to='app.customer', db_column='legacy_customer',
            ),
        ),
    ]
"#;

    const ADDFIELD_FK_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
        migrations.CreateModel(
            name='Order',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
            ],
        ),
    ]
"#;

    #[test]
    fn test_addfield_fk_before_createmodel_is_not_exempted() {
        // Order-aware exemption: a CreateModel that runs *after* the
        // AddField cannot retroactively make the AddField safe.
        let diagnostics = check_migration(ADDFIELD_FK_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    #[test]
    fn test_add_fk_with_db_column_and_prebuilt_index_still_flags() {
        // The conservative R006 policy is deliberately independent of
        // column-name matching: even if the prebuilt index targets the
        // physical db_column, the AddField(ForeignKey) rollout still
        // needs to be split explicitly.
        let diagnostics = check_migration(ADD_FK_WITH_DB_COLUMN_KNOWN_LIMITATION);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const WRAPPED_ADD_FK_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.AddField(
                    model_name='order',
                    name='customer',
                    field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_database_add_fk_is_flagged() {
        let diagnostics = check_migration(WRAPPED_ADD_FK_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const NESTED_SDAS_ADD_FK_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.SeparateDatabaseAndState(
                    database_operations=[
                        migrations.AddField(
                            model_name='order',
                            name='customer',
                            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
                        ),
                    ],
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_nested_sdas_database_add_fk_is_flagged() {
        let diagnostics = check_migration(NESTED_SDAS_ADD_FK_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const ADD_ONE_TO_ONE_EXISTING_MODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='profile',
            name='user',
            field=models.OneToOneField(on_delete=models.CASCADE, to='auth.user'),
        ),
    ]
"#;

    #[test]
    fn test_add_one_to_one_existing_model_bad() {
        let diagnostics = check_migration(ADD_ONE_TO_ONE_EXISTING_MODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    const ADD_FK_POSITIONAL_EXISTING_MODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            'order',
            'customer',
            models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    #[test]
    fn test_add_fk_positional_existing_model_bad() {
        let diagnostics = check_migration(ADD_FK_POSITIONAL_EXISTING_MODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }
}
