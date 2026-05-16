//! R002: Unique constraint locks the table
//!
//! Detects `migrations.AddConstraint(UniqueConstraint(...))` on an existing
//! table. Django's `AddConstraint` always builds the constraint's index from
//! scratch — it does not accept an existing concurrent index as a parameter.
//! The result is a `CREATE UNIQUE INDEX` (non-concurrent) that locks the
//! table for the duration of the scan.

use crate::ast::{ConstraintType, Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

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
        // Walk operations in order so the CreateModel exemption only
        // honors models that were actually created *before* the
        // AddConstraint in this migration. The previous implementation
        // used `migration.is_model_created`, which is order-blind: a
        // CreateModel placed *after* the AddConstraint silently
        // exempted, even though Django would execute the AddConstraint
        // first and lock the (then non-empty? still missing?) target.
        let mut diagnostics = Vec::new();
        let mut created_so_far: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for op in &migration.operations {
            match op.op_type {
                OperationType::CreateModel => {
                    if let OperationData::Model(m) = &op.data {
                        created_so_far.insert(m.name.to_lowercase());
                    }
                }
                OperationType::AddConstraint => {
                    let OperationData::Constraint(data) = &op.data else {
                        continue;
                    };
                    if data.constraint_type != ConstraintType::Unique {
                        continue;
                    }
                    if created_so_far.contains(&data.model_name.to_lowercase()) {
                        continue;
                    }

                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: "AddConstraint with UniqueConstraint locks the table while it builds the index"
                            .to_string(),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "Django's AddConstraint cannot reuse a pre-built index. \
                             To avoid the lock, build the index concurrently first and \
                             attach it via RunSQL:\n\
                             \n  \
                             RunSQL(\n    \
                                'CREATE UNIQUE INDEX CONCURRENTLY <name> ON <table> (<cols>);'\n    \
                                'ALTER TABLE <table> ADD CONSTRAINT <name> UNIQUE USING INDEX <name>;',\n  \
                             )\n\
                             \n\
                             The CREATE INDEX statement must run outside a transaction \
                             (`atomic = False`)."
                                .to_string(),
                        ),
                    });
                }
                _ => {}
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
}
