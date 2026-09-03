//! R010: AddField with NOT NULL and no default
//!
//! Detects AddField operations that add a NOT NULL field without a default value.
//! This requires an immediate rewrite of all rows to set the value, which locks the table.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{walk_with_created_models, Rule, RuleContext};

/// Rule that detects AddField with NOT NULL and no default.
pub struct R010AddFieldNotNull;

impl Rule for R010AddFieldNotNull {
    fn id(&self) -> &'static str {
        "R010"
    }

    fn name(&self) -> &'static str {
        "add-field-not-null"
    }

    fn description(&self) -> &'static str {
        "AddField with NOT NULL and no Python or database default requires rewriting all rows immediately. \
         Add the field as nullable first, backfill data, then make it NOT NULL."
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
            if created.contains_operation(migration, op) {
                return;
            }
            let Some(field) = &data.field else { return };
            if field.is_nullable || field.has_default || field.has_db_default {
                return;
            }

            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    if migration.framework == crate::discovery::MigrationFramework::Aerich {
                        format!(
                            "Adding '{}' as NOT NULL without a default can rewrite the table",
                            data.field_name
                        )
                    } else {
                        format!(
                            "AddField '{}' is NOT NULL without a Python or database default",
                            data.field_name
                        )
                    },
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(if migration.framework == crate::discovery::MigrationFramework::Aerich {
                    "Add the column as nullable, backfill it, then set NOT NULL in a later Aerich migration."
                } else {
                    "Either: 1) Add the field as nullable with null=True, backfill, then \
                     remove null=True in a separate migration, or 2) Provide a default value. \
                     A recognized db_default avoids this warning but is not a universal zero-downtime guarantee."
                }),
            );
        });

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOT_NULL_NO_DEFAULT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='sku',
            field=models.CharField(max_length=50),
        ),
    ]
"#;

    const NOT_NULL_WITH_DEFAULT_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='status',
            field=models.CharField(max_length=50, default='active'),
        ),
    ]
"#;

    const NULLABLE_GOOD: &str = r#"
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

    const NEW_MODEL_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[],
        ),
        migrations.AddField(
            model_name='product',
            name='sku',
            field=models.CharField(max_length=50),
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R010AddFieldNotNull, source)
    }

    #[test]
    fn test_addfield_notnull_without_default_is_flagged() {
        let diagnostics = check_migration(NOT_NULL_NO_DEFAULT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R010");
    }

    #[test]
    fn test_not_null_with_default_good() {
        let diagnostics = check_migration(NOT_NULL_WITH_DEFAULT_GOOD);
        assert!(diagnostics.is_empty());
    }

    const NOT_NULL_WITH_DB_DEFAULT_GOOD: &str = r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    operations = [
        migrations.AddField(
            model_name='product',
            name='status',
            field=models.CharField(max_length=50, db_default='active'),
        ),
    ]
"#;

    #[test]
    fn test_not_null_with_db_default_good() {
        assert!(check_migration(NOT_NULL_WITH_DB_DEFAULT_GOOD).is_empty());
    }

    const DEFAULT_NONE_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='status',
            field=models.CharField(max_length=50, default=None),
        ),
    ]
"#;

    #[test]
    fn test_default_none_does_not_count_as_default() {
        let diagnostics = check_migration(DEFAULT_NONE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R010");
    }

    const DB_DEFAULT_NONE_BAD: &str = r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    operations = [
        migrations.AddField(
            model_name='product',
            name='status',
            field=models.CharField(max_length=50, db_default=None),
        ),
    ]
"#;

    #[test]
    fn test_db_default_none_does_not_count_as_default() {
        let diagnostics = check_migration(DB_DEFAULT_NONE_BAD);
        assert_eq!(diagnostics[0].rule_id, "R010");
    }

    const NOT_PROVIDED_DEFAULT_BAD: &str = r#"
from django.db import migrations, models
from django.db.models import NOT_PROVIDED


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            'product',
            'status',
            models.CharField(max_length=50, default=NOT_PROVIDED),
        ),
    ]
"#;

    #[test]
    fn test_positional_addfield_with_not_provided_default_is_flagged() {
        let diagnostics = check_migration(NOT_PROVIDED_DEFAULT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R010");
    }

    #[test]
    fn test_nullable_good() {
        let diagnostics = check_migration(NULLABLE_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_new_model_exempt() {
        let diagnostics = check_migration(NEW_MODEL_GOOD);
        assert!(diagnostics.is_empty());
    }

    const ADDFIELD_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='sku',
            field=models.CharField(max_length=50),
        ),
        migrations.CreateModel(
            name='Product',
            fields=[],
        ),
    ]
"#;

    #[test]
    fn test_addfield_before_createmodel_is_not_exempted() {
        // Order-aware exemption: a CreateModel that runs *after*
        // the AddField cannot retroactively make the AddField safe.
        let diagnostics = check_migration(ADDFIELD_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R010");
    }

    const WRAPPED_NOT_NULL_NO_DEFAULT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.AddField(
                    model_name='product',
                    name='sku',
                    field=models.CharField(max_length=50),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_database_addfield_notnull_is_flagged() {
        let diagnostics = check_migration(WRAPPED_NOT_NULL_NO_DEFAULT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R010");
    }
}
