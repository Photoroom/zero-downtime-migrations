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
        "AddField with NOT NULL and no default requires rewriting all rows immediately. \
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
            if created.contains(&data.model_name) {
                return;
            }
            let Some(field) = &data.field else { return };
            if field.is_nullable || field.has_default {
                return;
            }

            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: format!(
                    "AddField '{}' is NOT NULL without a default value",
                    data.field_name
                ),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: op.span,
                help: Some(
                    "Either: 1) Add the field as nullable with null=True, backfill, then \
                     remove null=True in a separate migration, or 2) Provide a default value"
                        .to_string(),
                ),
            });
        });

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::MigrationExtractor;
    use crate::config::Config;
    use crate::parser::ParsedMigration;
    use std::path::Path;

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
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("test.py"),
        };
        R010AddFieldNotNull.check(&migration, &ctx)
    }

    #[test]
    fn test_not_null_no_default_bad() {
        let diagnostics = check_migration(NOT_NULL_NO_DEFAULT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R010");
    }

    #[test]
    fn test_not_null_with_default_good() {
        let diagnostics = check_migration(NOT_NULL_WITH_DEFAULT_GOOD);
        assert!(diagnostics.is_empty());
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
        // The previous `is_model_created` lookup was order-blind
        // and silently exempted this — the same false negative
        // R002/R006/R016/R017 fixed earlier in this PR. The README
        // even claimed R010 honoured the CreateModel exemption but
        // the implementation was order-blind.
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
