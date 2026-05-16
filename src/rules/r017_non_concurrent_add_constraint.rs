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

        for op in migration.operations_of_type(OperationType::AddConstraint) {
            let OperationData::Constraint(data) = &op.data else {
                continue;
            };
            // Skip if model was created in same migration — no rows
            // yet, so no validation lock and no live traffic to block
            // while building any index.
            if migration.is_model_created(&data.model_name) {
                continue;
            }

            // FKs go through AddField (covered by R006); UniqueConstraint
            // is covered by R002 with USING INDEX guidance.
            let (message, help) = match data.constraint_type {
                ConstraintType::Check => (
                    "AddConstraint with a CHECK constraint validates all rows".to_string(),
                    "Use RunSQL to add the constraint with NOT VALID, then validate \
                     in a separate migration:\n  \
                     ALTER TABLE ... ADD CONSTRAINT <name> CHECK (...) NOT VALID;\n  \
                     ALTER TABLE ... VALIDATE CONSTRAINT <name>;  -- table-scan without blocking writes"
                        .to_string(),
                ),
                ConstraintType::Exclusion => (
                    "AddConstraint with an EXCLUDE constraint builds its index \
                     non-concurrently, locking the table"
                        .to_string(),
                    "PostgreSQL has no NOT VALID form for EXCLUDE constraints, and \
                     `ALTER TABLE ... ADD CONSTRAINT ... EXCLUDE USING gist` always \
                     builds its own index under ACCESS EXCLUSIVE — `USING INDEX` is \
                     accepted only for UNIQUE and PRIMARY KEY, not EXCLUDE.\n\n\
                     There is no fully-online path in stock PostgreSQL. Mitigations:\n  \
                     - Defer the migration to a low-traffic window.\n  \
                     - Define the constraint at table creation (CreateModel) if the \
                     table is new.\n  \
                     - Enforce the rule with a trigger instead of a constraint while \
                     the table is live.\n\n\
                     If you accept the lock anyway, run with `atomic = False` and \
                     `SET lock_timeout` so a queued reader doesn't block all writes \
                     indefinitely."
                        .to_string(),
                ),
                _ => continue,
            };

            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message,
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: op.span,
                help: Some(help),
            });
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
    fn test_exclusion_constraint_bad() {
        // The previous test asserted no diagnostic with the comment
        // "Exclusion constraints don't require full table validation".
        // That's wrong: ExclusionConstraint builds its enforcement
        // index non-concurrently, which holds an ACCESS EXCLUSIVE lock
        // for the duration of the build. R017 must flag it.
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
}
