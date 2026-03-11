//! R009: SeparateDatabaseAndState followed by second step in same PR
//!
//! Detects when SeparateDatabaseAndState is used and there's another migration
//! in the same changeset that appears to be a follow-up step. The whole point
//! of SeparateDatabaseAndState is to deploy the steps separately.

use std::path::Path;

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{ChangesetRule, RuleContext};

/// Rule that detects SeparateDatabaseAndState followed by second step in same PR.
pub struct R009SeparateDbStateSamePr;

impl ChangesetRule for R009SeparateDbStateSamePr {
    fn id(&self) -> &'static str {
        "R009"
    }

    fn name(&self) -> &'static str {
        "separate-db-state-same-pr"
    }

    fn description(&self) -> &'static str {
        "When using SeparateDatabaseAndState, the state change and database change should \
         be in separate PRs/deployments. Having both in the same changeset defeats the purpose."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(
        &self,
        migrations: &[&Migration],
        _other_changed_files: &[&Path],
        _ctx: &RuleContext,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Find migrations with SeparateDatabaseAndState
        let separate_migrations: Vec<_> = migrations
            .iter()
            .filter(|m| {
                m.operations
                    .iter()
                    .any(|op| op.op_type == OperationType::SeparateDatabaseAndState)
            })
            .collect();

        if separate_migrations.is_empty() {
            return diagnostics;
        }

        // Check if there are multiple migrations in the changeset
        // that suggest a two-step deployment pattern
        for migration in &separate_migrations {
            // Check if the SeparateDatabaseAndState has only state_operations (step 1)
            // or only database_operations (step 2)
            for op in &migration.operations {
                if op.op_type == OperationType::SeparateDatabaseAndState {
                    if let OperationData::SeparateDatabaseAndState(data) = &op.data {
                        let is_state_only =
                            data.has_state_operations && !data.has_database_operations;
                        let is_db_only = data.has_database_operations && !data.has_state_operations;

                        // If this is step 1 (state_operations only) and there are other migrations,
                        // warn that they might be trying to do both steps in one PR
                        if is_state_only && migrations.len() > 1 {
                            diagnostics.push(Diagnostic {
                                rule_id: self.id(),
                                rule_name: self.name(),
                                message: "SeparateDatabaseAndState with state_operations alongside other migrations".to_string(),
                                severity: self.severity(),
                                path: migration.path.clone(),
                                span: op.span,
                                help: Some(
                                    "SeparateDatabaseAndState is meant for two-phase deployments: \
                                     1) Deploy state change, 2) Deploy database change separately. \
                                     Having both in one PR defeats this purpose."
                                        .to_string(),
                                ),
                                fix: None,
                            });
                        }

                        // If this is step 2 (database_operations only) and there's a step 1 migration
                        // in the same changeset, warn
                        if is_db_only {
                            let has_step1 = separate_migrations.iter().any(|m| {
                                m.operations.iter().any(|op2| {
                                    if let OperationData::SeparateDatabaseAndState(d) = &op2.data {
                                        d.has_state_operations && !d.has_database_operations
                                    } else {
                                        false
                                    }
                                })
                            });

                            if has_step1 {
                                diagnostics.push(Diagnostic {
                                    rule_id: self.id(),
                                    rule_name: self.name(),
                                    message: "Both state_operations and database_operations migrations in same changeset".to_string(),
                                    severity: self.severity(),
                                    path: migration.path.clone(),
                                    span: op.span,
                                    help: Some(
                                        "Deploy the state_operations migration first, wait for all \
                                         application servers to pick up the change, then deploy \
                                         the database_operations migration in a separate PR."
                                            .to_string(),
                                    ),
                                    fix: None,
                                });
                            }
                        }
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

    const STATE_ONLY_MIGRATION: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            state_operations=[
                migrations.RemoveField(
                    model_name='product',
                    name='deprecated_field',
                ),
            ],
        ),
    ]
"#;

    const DB_ONLY_MIGRATION: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RunSQL('DROP COLUMN deprecated_field'),
            ],
        ),
    ]
"#;

    const OTHER_MIGRATION: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='new_field',
            field=models.CharField(max_length=50, null=True),
        ),
    ]
"#;

    fn parse_migration(source: &str, path: &str) -> Migration {
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        extractor.extract(Path::new(path)).unwrap()
    }

    #[test]
    fn test_state_and_db_migrations_same_pr_warns() {
        let state_migration = parse_migration(STATE_ONLY_MIGRATION, "0001_state.py");
        let db_migration = parse_migration(DB_ONLY_MIGRATION, "0002_db.py");
        let migrations = vec![&state_migration, &db_migration];
        let other_files: Vec<&Path> = vec![];
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R009SeparateDbStateSamePr.check(&migrations, &other_files, &ctx);

        // Should warn about both steps being in same PR
        assert!(!diagnostics.is_empty());
        assert!(diagnostics.iter().all(|d| d.rule_id == "R009"));
    }

    #[test]
    fn test_state_only_with_other_migration_warns() {
        let state_migration = parse_migration(STATE_ONLY_MIGRATION, "0001_state.py");
        let other_migration = parse_migration(OTHER_MIGRATION, "0002_other.py");
        let migrations = vec![&state_migration, &other_migration];
        let other_files: Vec<&Path> = vec![];
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R009SeparateDbStateSamePr.check(&migrations, &other_files, &ctx);

        // Should warn about state migration alongside other migrations
        assert!(!diagnostics.is_empty());
    }

    #[test]
    fn test_single_state_migration_good() {
        let state_migration = parse_migration(STATE_ONLY_MIGRATION, "0001_state.py");
        let migrations = vec![&state_migration];
        let other_files: Vec<&Path> = vec![];
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R009SeparateDbStateSamePr.check(&migrations, &other_files, &ctx);

        // Single migration is fine
        assert!(diagnostics.is_empty());
    }
}
