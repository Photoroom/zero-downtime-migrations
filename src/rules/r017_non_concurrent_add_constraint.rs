//! R017: Non-concurrent AddConstraint
//!
//! Detects `AddConstraint` operations that hold a table lock while
//! building or validating the constraint:
//!
//!   - `CheckConstraint` — validating the predicate against every
//!     existing row, blocking writes for the duration.
//!   - `ExclusionConstraint` — building its enforcement index
//!     non-concurrently, blocking writes for the build.
//!
//! `UniqueConstraint` has the same problem (the index it builds is
//! non-concurrent), but R002 already flags it with specific guidance
//! about `USING INDEX`, so we leave it to R002 to avoid double-firing.
//!
//! `ConstraintType::Unknown` (a constraint class we couldn't classify
//! from the source) is silently skipped to avoid false positives on
//! unrecognised classes.

use crate::ast::{ConstraintType, Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{walk_with_created_models, Rule, RuleContext};

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
        "AddConstraint with a CHECK or EXCLUDE constraint locks the table — CHECK \
         validates every row, EXCLUDE builds its enforcement index non-concurrently. \
         Migrate CHECK via NOT VALID + VALIDATE. EXCLUDE has no fully-online path \
         in stock PostgreSQL — defer it to a low-traffic window."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        walk_with_created_models(migration, |op, created| {
            if op.op_type != OperationType::AddConstraint {
                return;
            }
            let OperationData::Constraint(data) = &op.data else {
                return;
            };
            if created.contains_operation(migration, op) {
                return;
            }
            if migration.framework == crate::discovery::MigrationFramework::Alembic
                && data.not_valid
                && matches!(
                    data.constraint_type,
                    ConstraintType::Check | ConstraintType::ForeignKey
                )
            {
                return;
            }

            // Django FKs go through AddField (covered by R006); Alembic
            // represents one directly as a constraint. UniqueConstraint
            // is covered by R002 with USING INDEX guidance.
            let (message, help) = match data.constraint_type {
                ConstraintType::Check => (
                    if migration.framework == crate::discovery::MigrationFramework::Alembic {
                        "Adding a CHECK constraint validates all existing rows".to_string()
                    } else {
                        "AddConstraint with a CHECK constraint validates all rows".to_string()
                    },
                    if migration.framework == crate::discovery::MigrationFramework::Alembic {
                        "Use op.execute to add the constraint as NOT VALID, then validate it in a later revision.".to_string()
                    } else {
                        include_str!("help/r017_check_constraint.txt").to_string()
                    },
                ),
                ConstraintType::Exclusion => (
                    if migration.framework == crate::discovery::MigrationFramework::Alembic {
                        "Adding an EXCLUDE constraint builds its index non-concurrently, locking the table".to_string()
                    } else {
                        "AddConstraint with an EXCLUDE constraint builds its index non-concurrently, locking the table".to_string()
                    },
                    if migration.framework == crate::discovery::MigrationFramework::Alembic {
                        "PostgreSQL has no fully-online EXCLUDE constraint path; create it with a new table or use a low-traffic window.".to_string()
                    } else {
                        include_str!("help/r017_exclusion_constraint.txt").to_string()
                    },
                ),
                ConstraintType::ForeignKey => (
                    "Adding a FOREIGN KEY validates all existing rows".to_string(),
                    "Add the foreign key as NOT VALID, validate it separately, then enforce it after the application is ready.".to_string(),
                ),
                _ => return,
            };

            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    message,
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(help),
            );
        });

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    const EXCLUSION_CONSTRAINT_BAD: &str = r#"
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

    const EXCLUSION_CONSTRAINT_ON_FRESH_MODEL_GOOD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.constraints import ExclusionConstraint


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Booking',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
                ('daterange', models.DateRangeField()),
            ],
        ),
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
        crate::rules::test_support::check_rule(&R017NonConcurrentAddConstraint, source)
    }

    #[test]
    fn test_check_constraint_warns() {
        let diagnostics = check_migration(CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    #[test]
    fn test_exclusion_constraint_bad() {
        // ExclusionConstraint builds its enforcement index non-concurrently,
        // holding an ACCESS EXCLUSIVE lock for the build, so R017 must flag it.
        let diagnostics = check_migration(EXCLUSION_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
        assert!(
            diagnostics[0].message.contains("EXCLUDE"),
            "message should mention EXCLUDE, got: {}",
            diagnostics[0].message
        );
    }

    #[test]
    fn test_exclusion_constraint_on_fresh_model_is_exempt() {
        // Same CreateModel exemption as CheckConstraint: a freshly
        // created table has no rows yet, so the lock is harmless.
        let diagnostics = check_migration(EXCLUSION_CONSTRAINT_ON_FRESH_MODEL_GOOD);
        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:?}",
        );
    }

    #[test]
    fn test_create_model_with_check_constraint_exempt() {
        // CheckConstraint on a model created in same migration should be exempt
        let diagnostics = check_migration(CREATE_MODEL_WITH_CHECK_CONSTRAINT);
        assert!(diagnostics.is_empty());
    }

    const ADDCONSTRAINT_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('price', models.DecimalField()),
            ],
        ),
    ]
"#;

    #[test]
    fn test_addconstraint_before_createmodel_is_not_exempted() {
        // Order-aware exemption: a CreateModel that runs *after* the
        // AddConstraint cannot retroactively make the AddConstraint safe.
        let diagnostics = check_migration(ADDCONSTRAINT_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    const WRAPPED_CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.AddConstraint(
                    model_name='product',
                    constraint=models.CheckConstraint(
                        check=models.Q(price__gte=0),
                        name='positive_price',
                    ),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_database_check_constraint_is_flagged() {
        let diagnostics = check_migration(WRAPPED_CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    const NESTED_SDAS_CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.SeparateDatabaseAndState(
                    database_operations=[
                        migrations.AddConstraint(
                            model_name='product',
                            constraint=models.CheckConstraint(
                                check=models.Q(price__gte=0),
                                name='positive_price',
                            ),
                        ),
                    ],
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_nested_sdas_database_check_constraint_is_flagged() {
        let diagnostics = check_migration(NESTED_SDAS_CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    const CUSTOM_CONSTRAINT_NAME_CONTAINS_CHECK_GOOD: &str = r#"
from django.db import migrations


class MyCheckConstraintLikeThing:
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=MyCheckConstraintLikeThing(),
        ),
    ]
"#;

    #[test]
    fn test_custom_constraint_with_check_in_name_is_not_classified() {
        let diagnostics = check_migration(CUSTOM_CONSTRAINT_NAME_CONTAINS_CHECK_GOOD);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
    }

    const POSITIONAL_CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            'product',
            models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
    ]
"#;

    #[test]
    fn test_positional_check_constraint_warns() {
        let diagnostics = check_migration(POSITIONAL_CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }
}
