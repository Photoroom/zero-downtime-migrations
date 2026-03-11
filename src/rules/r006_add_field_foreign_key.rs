//! R006: AddField with ForeignKey
//!
//! Detects AddField operations that add a ForeignKey. Foreign keys
//! require exclusive locks and implicit index creation. Should use
//! SeparateDatabaseAndState to add the column first, then the constraint.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects AddField with ForeignKey.
pub struct R006AddFieldForeignKey;

impl Rule for R006AddFieldForeignKey {
    fn id(&self) -> &'static str {
        "R006"
    }

    fn name(&self) -> &'static str {
        "add-field-foreign-key"
    }

    fn description(&self) -> &'static str {
        "AddField with ForeignKey creates an implicit index and constraint in one step, \
         causing table locks. Use SeparateDatabaseAndState to split into multiple steps."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Note: We only extract top-level operations. If an AddField is properly
        // wrapped inside SeparateDatabaseAndState, it won't be extracted as a
        // separate operation. Therefore, any AddField we see here is NOT wrapped
        // and should be checked.

        for op in migration.operations_of_type(OperationType::AddField) {
            if let OperationData::Field(data) = &op.data {
                if let Some(ref field) = data.field {
                    if field.field_type == "ForeignKey" {
                        // Exempt if the model was just created in this migration
                        if migration.is_model_created(&data.model_name) {
                            continue;
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
                            help: Some(
                                "Consider using SeparateDatabaseAndState to: \
                                 1) Add nullable column without FK constraint, \
                                 2) Backfill data, \
                                 3) Add constraint and index concurrently"
                                    .to_string(),
                            ),
                            fix: None,
                        });
                    }
                }
            }
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

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("test.py"),
        };
        R006AddFieldForeignKey.check(&migration, &ctx)
    }

    #[test]
    fn test_add_fk_existing_model_bad() {
        let diagnostics = check_migration(ADD_FK_EXISTING_MODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }

    #[test]
    fn test_add_fk_new_model_good() {
        // FK on newly created model is exempt
        let diagnostics = check_migration(ADD_FK_NEW_MODEL_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_add_non_fk_good() {
        let diagnostics = check_migration(ADD_NON_FK_GOOD);
        assert!(diagnostics.is_empty());
    }
}
