//! R007: ForeignKey without concurrent index
//!
//! Detects ForeignKey additions that don't use a concurrent index strategy.
//! When adding a FK to an existing table, the implicit index creation
//! locks the table. Should create index concurrently first.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects ForeignKey without concurrent index.
pub struct R007FKWithoutIndex;

impl Rule for R007FKWithoutIndex {
    fn id(&self) -> &'static str {
        "R007"
    }

    fn name(&self) -> &'static str {
        "fk-without-concurrent-index"
    }

    fn description(&self) -> &'static str {
        "ForeignKey fields create an implicit index. When adding to an existing table, \
         this index creation locks the table. Create the index concurrently first."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Collect models that have concurrent index operations
        let models_with_concurrent_index: std::collections::HashSet<String> = migration
            .operations
            .iter()
            .filter(|op| op.op_type == OperationType::AddIndexConcurrently)
            .filter_map(|op| {
                if let OperationData::Index(idx) = &op.data {
                    Some(idx.model_name.to_lowercase())
                } else {
                    None
                }
            })
            .collect();

        for op in migration.operations_of_type(OperationType::AddField) {
            if let OperationData::Field(data) = &op.data {
                if let Some(ref field) = data.field {
                    if field.field_type == "ForeignKey" {
                        // Exempt if the model was just created in this migration
                        if migration.is_model_created(&data.model_name) {
                            continue;
                        }

                        // Exempt if there's a concurrent index for the same model
                        if models_with_concurrent_index.contains(&data.model_name.to_lowercase()) {
                            continue;
                        }

                        diagnostics.push(Diagnostic {
                            rule_id: self.id(),
                            rule_name: self.name(),
                            message: format!(
                                "ForeignKey '{}' on '{}' may benefit from pre-created concurrent index",
                                data.field_name, data.model_name
                            ),
                            severity: self.severity(),
                            path: ctx.path.to_path_buf(),
                            span: op.span,
                            help: Some(
                                "Consider creating the index first with AddIndexConcurrently, \
                                 then adding the ForeignKey constraint in a separate step"
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

    const FK_WITHOUT_CONCURRENT_INDEX_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    const FK_WITH_CONCURRENT_INDEX_GOOD: &str = r#"
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

    const FK_ON_NEW_MODEL_GOOD: &str = r#"
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
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
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
        R007FKWithoutIndex.check(&migration, &ctx)
    }

    #[test]
    fn test_fk_without_concurrent_index_bad() {
        let diagnostics = check_migration(FK_WITHOUT_CONCURRENT_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R007");
    }

    #[test]
    fn test_fk_with_concurrent_index_good() {
        let diagnostics = check_migration(FK_WITH_CONCURRENT_INDEX_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_fk_on_new_model_good() {
        // FK on newly created model is exempt
        let diagnostics = check_migration(FK_ON_NEW_MODEL_GOOD);
        assert!(diagnostics.is_empty());
    }
}
