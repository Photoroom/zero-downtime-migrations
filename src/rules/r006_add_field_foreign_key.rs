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
            if !field.is_relation {
                return;
            }

            if created.contains(&data.model_name) {
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
        assert!(check_migration(ADD_FK_NEW_MODEL_GOOD).is_empty());
    }

    #[test]
    fn test_add_non_fk_good() {
        assert!(check_migration(ADD_NON_FK_GOOD).is_empty());
    }

    #[test]
    fn test_prebuilt_index_does_not_exempt_fk() {
        let diagnostics = check_migration(ADD_FK_AFTER_CONCURRENT_INDEX_GOOD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

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
