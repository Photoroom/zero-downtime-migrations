//! R017: Non-concurrent AddConstraint
//!
//! Detects AddConstraint operations that add constraints requiring full table scans.
//! Consider adding constraints with NOT VALID then validating separately.

use crate::ast::{ConstraintType, Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects constraints that may cause table locks.
pub struct R017NonConcurrentAddConstraint;

impl Rule for R017NonConcurrentAddConstraint {
    fn id(&self) -> &'static str {
        "R017"
    }

    fn name(&self) -> &'static str {
        "non-concurrent-add-constraint"
    }

    fn description(&self) -> &'static str {
        "AddConstraint with CHECK or ForeignKey validates all existing rows, which locks \
         the table. Add constraints with NOT VALID then VALIDATE separately."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::AddConstraint) {
            if let OperationData::Constraint(data) = &op.data {
                // Skip if model was created in same migration - no existing rows to validate
                if migration.is_model_created(&data.model_name) {
                    continue;
                }

                // Check and ForeignKey constraints require full table validation
                if matches!(
                    data.constraint_type,
                    ConstraintType::Check | ConstraintType::ForeignKey
                ) {
                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: format!(
                            "AddConstraint with {:?} validates all rows",
                            data.constraint_type
                        ),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "Consider using RunSQL to add the constraint with NOT VALID, \
                             then validate in a separate migration: \
                             ALTER TABLE ADD CONSTRAINT ... NOT VALID; then \
                             ALTER TABLE VALIDATE CONSTRAINT ..."
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

    const CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
    ]
"#;

    const EXCLUSION_CONSTRAINT_GOOD: &str = r#"
from django.db import migrations
from django.contrib.postgres.constraints import ExclusionConstraint


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='booking',
            constraint=ExclusionConstraint(
                name='exclude_overlapping',
                expressions=[('daterange', '&&')],
            ),
        ),
    ]
"#;

    const CREATE_MODEL_WITH_CHECK_CONSTRAINT: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('price', models.DecimalField()),
            ],
        ),
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
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
        R017NonConcurrentAddConstraint.check(&migration, &ctx)
    }

    #[test]
    fn test_check_constraint_warns() {
        let diagnostics = check_migration(CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    #[test]
    fn test_exclusion_constraint_good() {
        // Exclusion constraints don't require full table validation
        let diagnostics = check_migration(EXCLUSION_CONSTRAINT_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_create_model_with_check_constraint_exempt() {
        // CheckConstraint on a model created in same migration should be exempt
        let diagnostics = check_migration(CREATE_MODEL_WITH_CHECK_CONSTRAINT);
        assert!(diagnostics.is_empty());
    }
}
