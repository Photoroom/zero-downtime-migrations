//! R002: Unique constraint without concurrent index
//!
//! Detects AddConstraint with UniqueConstraint that doesn't have a corresponding
//! concurrent index already created. Adding a unique constraint directly
//! requires scanning the entire table with a lock.

use crate::ast::{ConstraintType, Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects unique constraints without pre-built concurrent indexes.
pub struct R002UniqueConstraintWithoutIndex;

impl Rule for R002UniqueConstraintWithoutIndex {
    fn id(&self) -> &'static str {
        "R002"
    }

    fn name(&self) -> &'static str {
        "unique-constraint-without-index"
    }

    fn description(&self) -> &'static str {
        "Adding a UniqueConstraint directly requires a full table scan with locks. \
         First create a unique index concurrently, then add the constraint using that index."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::AddConstraint) {
            if let OperationData::Constraint(data) = &op.data {
                // Skip if model was created in same migration - no existing rows to lock
                if migration.is_model_created(&data.model_name) {
                    continue;
                }

                if data.constraint_type == ConstraintType::Unique {
                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: "UniqueConstraint added without pre-built concurrent index"
                            .to_string(),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "First create the unique index using AddIndexConcurrently, \
                             then add the constraint referencing that index"
                                .to_string(),
                        ),
                        fix: None,
                    });
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

    const UNIQUE_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.UniqueConstraint(fields=['sku'], name='unique_sku'),
        ),
    ]
"#;

    const CHECK_CONSTRAINT_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
    ]
"#;

    const CREATE_MODEL_WITH_UNIQUE_CONSTRAINT: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('sku', models.CharField(max_length=50)),
            ],
        ),
        migrations.AddConstraint(
            model_name='product',
            constraint=models.UniqueConstraint(fields=['sku'], name='unique_sku'),
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
        R002UniqueConstraintWithoutIndex.check(&migration, &ctx)
    }

    #[test]
    fn test_unique_constraint_bad() {
        let diagnostics = check_migration(UNIQUE_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R002");
    }

    #[test]
    fn test_check_constraint_good() {
        let diagnostics = check_migration(CHECK_CONSTRAINT_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_create_model_with_unique_constraint_exempt() {
        // UniqueConstraint on a model created in same migration should be exempt
        let diagnostics = check_migration(CREATE_MODEL_WITH_UNIQUE_CONSTRAINT);
        assert!(diagnostics.is_empty());
    }
}
