//! R006: AddField with ForeignKey on an existing model.
//!
//! Adding a ForeignKey via `AddField` against an existing table is doubly
//! expensive: it builds the index implicitly (acquiring a lock for the
//! duration) and validates the constraint against every existing row.
//!
//! This rule was originally split across R006 (FK locks the table) and
//! R007 (FK should be preceded by a concurrent index). They fired on the
//! same operation with overlapping prescriptions; R007 was retired and
//! its order-aware concurrent-index exemption is now part of R006.

use crate::ast::{Migration, OperationData, OperationType};
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
         constraint against every existing row. Pre-create the index with \
         AddIndexConcurrently earlier in the migration, or split the work via \
         SeparateDatabaseAndState."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        // R006 needs an extra piece of state on top of the standard
        // `created_so_far` walk: `leading_columns` tracks concurrent
        // indexes that appeared *before* the AddField and lead with
        // the FK column. Postgres can use a btree's leading prefix
        // for FK lookups/joins but not a non-leading column, so an
        // index on `(status, customer)` does nothing for an FK on
        // `customer`. The closure captures both — `created_so_far`
        // comes from the helper, `leading_columns` is local state
        // the closure mutates as it iterates.
        //
        // Known simplification: this exemption assumes the prior
        // concurrent index is a plain btree without a `condition=`
        // (partial) or `opclasses=` set. A partial index on the FK
        // column doesn't help if the FK target row is filtered out
        // of the index, and a hash/GiST/GIN index can't satisfy the
        // FK enforcement lookup. The extractor doesn't surface those
        // attributes yet, so a savvy attacker who adds an unrelated
        // partial concurrent index could still trigger the
        // exemption. In practice the AddIndexConcurrently →
        // AddField(FK) pattern is almost always a plain btree;
        // tightening this is tracked for a follow-up alongside
        // partial/expression index extraction.
        let mut diagnostics = Vec::new();
        let mut leading_columns: std::collections::HashMap<
            String,
            std::collections::HashSet<String>,
        > = std::collections::HashMap::new();

        walk_with_created_models(migration, |op, created| {
            // First: record any concurrent-index op we see, so
            // subsequent AddFields can consult `leading_columns`.
            if op.op_type == OperationType::AddIndexConcurrently {
                if let OperationData::Index(idx) = &op.data {
                    if let Some(first) = idx.columns.first() {
                        leading_columns
                            .entry(idx.model_name.to_lowercase())
                            .or_default()
                            .insert(normalize_index_column(first));
                    }
                }
                return;
            }

            if op.op_type != OperationType::AddField {
                return;
            }
            let OperationData::Field(data) = &op.data else {
                return;
            };
            let Some(field) = &data.field else { return };
            if field.field_type != "ForeignKey" {
                return;
            }

            if created.contains(&data.model_name) {
                return;
            }
            // Django uses lowercase column names; we also accept
            // the auto-suffixed `<name>_id` form, since FK columns
            // in Postgres carry that suffix and the user may have
            // indexed either spelling.
            let model = data.model_name.to_lowercase();
            let field_name = data.field_name.to_lowercase();
            let fk_column = format!("{field_name}_id");
            if leading_columns
                .get(&model)
                .is_some_and(|leads| leads.contains(&field_name) || leads.contains(&fk_column))
            {
                return;
            }

            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: format!(
                    "AddField with ForeignKey on existing model '{}'",
                    data.model_name
                ),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: op.span,
                help: Some(include_str!("help/r006_add_fk.txt").to_string()),
            });
        });

        diagnostics
    }
}

fn normalize_index_column(column: &str) -> String {
    column.trim_start_matches('-').to_lowercase()
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
    fn test_add_fk_after_concurrent_index_good() {
        let diagnostics = check_migration(ADD_FK_AFTER_CONCURRENT_INDEX_GOOD);
        assert!(diagnostics.is_empty());
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
    fn test_add_fk_after_concurrent_index_on_fk_id_column_good() {
        // Postgres FK columns get a `<name>_id` suffix, so a user may
        // index either `customer` (Django's field name) or
        // `customer_id` (the actual SQL column). Both should exempt.
        let diagnostics = check_migration(ADD_FK_AFTER_CONCURRENT_INDEX_ON_FK_ID_COLUMN_GOOD);
        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:?}",
        );
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
    fn test_add_fk_after_descending_concurrent_index_good() {
        let diagnostics = check_migration(ADD_FK_AFTER_DESCENDING_CONCURRENT_INDEX_GOOD);
        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:?}",
        );
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
    fn test_add_fk_after_multi_column_index_leading_with_fk_good() {
        // A composite index on `(customer, status)` is usable for FK
        // joins/lookups on `customer` because Postgres can use any
        // leading-prefix of a btree.
        let diagnostics = check_migration(ADD_FK_AFTER_MULTI_COLUMN_INDEX_GOOD);
        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:?}",
        );
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
        // Per-FK granularity: the `customer` FK is exempt because of
        // the prior concurrent index, but the `product` FK has no
        // matching index and must still be flagged.
        let diagnostics = check_migration(TWO_FKS_ONLY_ONE_COVERED);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
        assert!(
            diagnostics[0].message.contains("order"),
            "diagnostic should be on the order model, got: {}",
            diagnostics[0].message
        );
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
        // AddField cannot retroactively make the AddField safe. The
        // previous `is_model_created` lookup was order-blind and
        // silently exempted this — the same false negative R002 and
        // R016 fixed earlier in this PR.
        let diagnostics = check_migration(ADDFIELD_FK_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    #[test]
    fn test_add_fk_with_db_column_is_false_positive() {
        // Known limitation: the extractor does not capture `db_column`,
        // so an index on the real SQL column (`legacy_customer`)
        // doesn't match the field name (`customer`) or its `_id`
        // suffix. R006 currently flags this even though the user
        // pre-built a covering index. Pinning the false-positive
        // behavior so a future `db_column`-aware extractor forces a
        // re-think instead of silently passing.
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
}
