//! R002: Unique constraint locks the table
//!
//! Detects `migrations.AddConstraint(UniqueConstraint(...))` on an existing
//! table. Django's `AddConstraint` always builds the constraint's index from
//! scratch — it does not accept an existing concurrent index as a parameter.
//! The result is a `CREATE UNIQUE INDEX` (non-concurrent) that locks the
//! table for the duration of the scan.

use crate::ast::{ConstraintType, Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{walk_with_created_models, Rule, RuleContext};

/// Rule that detects `AddConstraint(UniqueConstraint)` on existing tables,
/// where Django builds the index non-concurrently under a table lock.
pub struct R002UniqueConstraintWithoutIndex;

impl Rule for R002UniqueConstraintWithoutIndex {
    fn id(&self) -> &'static str {
        "R002"
    }

    fn name(&self) -> &'static str {
        "unique-constraint-without-index"
    }

    fn description(&self) -> &'static str {
        "AddConstraint with a UniqueConstraint builds the constraint's index \
         non-concurrently, locking the table for the duration of the scan. \
         Django's AddConstraint cannot reuse a pre-built index — issue the \
         constraint via RunSQL instead so it can attach to a concurrently \
         created index."
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
            if data.constraint_type != ConstraintType::Unique {
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
                    "AddConstraint with UniqueConstraint locks the table while it builds the index",
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(include_str!("help/r002_unique_constraint.txt")),
            );
        });

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        crate::rules::test_support::check_rule(&R002UniqueConstraintWithoutIndex, source)
    }

    #[test]
    fn test_addconstraint_unique_on_existing_model_is_flagged() {
        let diagnostics = check_migration(UNIQUE_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R002");
    }

    const POSITIONAL_UNIQUE_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            'product',
            models.UniqueConstraint(fields=['sku'], name='unique_sku'),
        ),
    ]
"#;

    #[test]
    fn test_positional_unique_constraint_is_flagged() {
        let diagnostics = check_migration(POSITIONAL_UNIQUE_CONSTRAINT_BAD);
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

    const ADDCONSTRAINT_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.UniqueConstraint(fields=['sku'], name='unique_sku'),
        ),
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('sku', models.CharField(max_length=50)),
            ],
        ),
    ]
"#;

    #[test]
    fn test_addconstraint_before_createmodel_is_not_exempted() {
        // The previous order-blind `is_model_created` exemption silently
        // passed this migration even though the AddConstraint executes
        // *before* the CreateModel and therefore can't be exempted by a
        // model that doesn't yet exist (or — if the model did exist in a
        // prior migration — locks it with rows in it).
        let diagnostics = check_migration(ADDCONSTRAINT_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R002");
    }

    #[test]
    fn test_help_text_recommends_separate_database_and_state_wrap() {
        // The original help told users to swap the AddConstraint for
        // two raw RunSQL operations — which gets the lock-avoidance
        // right but leaves Django's model state ignorant of the
        // constraint. The next `makemigrations` then regenerates the
        // AddConstraint and re-introduces the very pattern R002
        // exists to flag. The fix recommends wrapping the RunSQLs in
        // SeparateDatabaseAndState with the original AddConstraint
        // in `state_operations`. Pin both pieces so a future help-
        // text rewrite can't silently lose either half.
        let diagnostics = check_migration(UNIQUE_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        let help = diagnostics[0].help.as_ref().expect("R002 emits help");
        assert!(
            help.contains("SeparateDatabaseAndState"),
            "help should recommend the SDaS wrapper, got:\n{help}",
        );
        assert!(
            help.contains("state_operations") && help.contains("database_operations"),
            "help should show both halves of the SDaS wrap, got:\n{help}",
        );
    }

    const UNIQUE_CONSTRAINT_IN_WRAPPED_DATABASE_OPS_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.AddConstraint(
                    model_name='product',
                    constraint=models.UniqueConstraint(fields=['sku'], name='unique_sku'),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_database_unique_constraint_is_flagged() {
        let diagnostics = check_migration(UNIQUE_CONSTRAINT_IN_WRAPPED_DATABASE_OPS_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R002");
    }
}
