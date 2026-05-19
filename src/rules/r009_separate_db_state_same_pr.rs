//! R009: SeparateDatabaseAndState followed by second step in same PR
//!
//! Detects when SeparateDatabaseAndState is used and there's another migration
//! in the same changeset that appears to be a follow-up step. The whole point
//! of SeparateDatabaseAndState is to deploy the steps separately.

use std::path::Path;

use crate::ast::{Migration, OperationData};
use crate::diagnostics::{Diagnostic, Severity, Span};
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
        Severity::Error
    }

    fn check(
        &self,
        migrations: &[&Migration],
        _other_changed_files: &[&Path],
        _ctx: &RuleContext,
    ) -> Vec<Diagnostic> {
        // The rule only triggers when the changeset contains BOTH halves of
        // a SeparateDatabaseAndState two-step deployment: at least one
        // state-only migration AND at least one database-only migration.
        // We then flag every file participating in the pair, with one
        // diagnostic per file. Bundling a state-only migration with an
        // unrelated migration is intentionally not flagged here — that is
        // a separate bundling concern, not what R009 is about.
        let mut diagnostics = Vec::new();

        let kinds: Vec<Vec<SeparationKind>> =
            migrations.iter().map(|m| separation_kinds(m)).collect();
        let has_state_step = kinds
            .iter()
            .flatten()
            .any(|kind| *kind == SeparationKind::StateOnly);
        let has_db_step = kinds
            .iter()
            .flatten()
            .any(|kind| *kind == SeparationKind::DatabaseOnly);
        if !(has_state_step && has_db_step) {
            return diagnostics;
        }

        for (migration, kind) in migrations.iter().zip(&kinds) {
            if !kind.iter().any(|kind| {
                matches!(
                    kind,
                    SeparationKind::StateOnly | SeparationKind::DatabaseOnly
                )
            }) {
                continue;
            }
            let Some(span) = separation_span(migration) else {
                continue;
            };
            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: "Both halves of a SeparateDatabaseAndState two-step deployment \
                     appear in this changeset"
                    .to_string(),
                severity: self.severity(),
                path: migration.path.clone(),
                span,
                help: Some(
                    "Deploy the state-only migration first, wait for all application \
                     servers to pick up the change, then deploy the database-only \
                     migration in a separate PR."
                        .to_string(),
                ),
            });
        }

        diagnostics
    }
}

/// What kind of `SeparateDatabaseAndState` arm(s) a migration
/// contains, if any. R009 fires only when one PR holds *both*
/// `StateOnly` and `DatabaseOnly` migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeparationKind {
    /// Has `state_operations` but no `database_operations`.
    StateOnly,
    /// Has `database_operations` but no `state_operations`.
    DatabaseOnly,
    /// Has both arms — i.e. the SDaS is self-contained, not a
    /// two-step deployment. Not flagged.
    Both,
}

fn separation_kinds(migration: &Migration) -> Vec<SeparationKind> {
    migration
        .operations
        .iter()
        .filter_map(|op| match &op.data {
            OperationData::SeparateDatabaseAndState(d) => {
                match (d.has_state_operations, d.has_database_operations) {
                    (true, false) => Some(SeparationKind::StateOnly),
                    (false, true) => Some(SeparationKind::DatabaseOnly),
                    (true, true) => Some(SeparationKind::Both),
                    (false, false) => None,
                }
            }
            _ => None,
        })
        .collect()
}

/// Span of the migration's `SeparateDatabaseAndState` operation, used to
/// anchor the diagnostic. Returns `None` if no such operation exists
/// (in which case the migration wouldn't be flagged in the first place).
fn separation_span(migration: &Migration) -> Option<Span> {
    migration
        .operations
        .iter()
        .find(|op| {
            matches!(
                &op.data,
                OperationData::SeparateDatabaseAndState(d)
                    if d.has_state_operations ^ d.has_database_operations
            )
        })
        .map(|op| op.span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::MigrationExtractor;
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

    const STATE_ONLY_NON_LITERAL_MIGRATION: &str = r#"
from django.db import migrations


STATE_OPS = [
    migrations.RemoveField(
        model_name='product',
        name='deprecated_field',
    ),
]


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            state_operations=STATE_OPS,
        ),
    ]
"#;

    const DB_ONLY_NON_LITERAL_MIGRATION: &str = r#"
from django.db import migrations


DB_OPS = [
    migrations.RunSQL('DROP COLUMN deprecated_field'),
]


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=DB_OPS,
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

    const BOTH_THEN_STATE_MIGRATION: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            state_operations=[
                migrations.AlterModelTable(name='product', table='store_product'),
            ],
            database_operations=[
                migrations.RunSQL('ALTER TABLE product RENAME TO store_product'),
            ],
        ),
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

    fn parse_migration(source: &str, path: &str) -> Migration {
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        extractor.extract(Path::new(path)).unwrap()
    }

    fn run(migrations: &[&Migration]) -> Vec<Diagnostic> {
        let other_files: Vec<&Path> = vec![];
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };
        R009SeparateDbStateSamePr.check(migrations, &other_files, &ctx)
    }

    #[test]
    fn test_state_and_db_migrations_same_pr_emits_one_per_file() {
        let state_migration = parse_migration(STATE_ONLY_MIGRATION, "0001_state.py");
        let db_migration = parse_migration(DB_ONLY_MIGRATION, "0002_db.py");
        let migrations = vec![&state_migration, &db_migration];

        let diagnostics = run(&migrations);

        // One diagnostic per file in the pair (not two on one file or a
        // mix of differently-worded messages on each).
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics.iter().all(|d| d.rule_id == "R009"));
        let paths: std::collections::HashSet<_> =
            diagnostics.iter().map(|d| d.path.clone()).collect();
        assert!(paths.contains(&Path::new("0001_state.py").to_path_buf()));
        assert!(paths.contains(&Path::new("0002_db.py").to_path_buf()));
        // Same message on both, since they describe the same logical issue.
        let messages: std::collections::HashSet<_> =
            diagnostics.iter().map(|d| d.message.clone()).collect();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_non_literal_state_and_db_arms_still_count_as_present() {
        let state_migration = parse_migration(STATE_ONLY_NON_LITERAL_MIGRATION, "0001_state.py");
        let db_migration = parse_migration(DB_ONLY_NON_LITERAL_MIGRATION, "0002_db.py");
        let migrations = vec![&state_migration, &db_migration];

        let diagnostics = run(&migrations);

        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics.iter().all(|d| d.rule_id == "R009"));
    }

    #[test]
    fn test_state_only_with_unrelated_migration_does_not_warn() {
        // R009 is about the state/db pair specifically. Bundling a
        // state-only migration with an unrelated AddField is a different
        // concern (and the old rule's "any other migration" branch was
        // overbroad).
        let state_migration = parse_migration(STATE_ONLY_MIGRATION, "0001_state.py");
        let other_migration = parse_migration(OTHER_MIGRATION, "0002_other.py");
        let migrations = vec![&state_migration, &other_migration];

        let diagnostics = run(&migrations);

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_later_state_only_operation_is_classified() {
        let state_migration = parse_migration(BOTH_THEN_STATE_MIGRATION, "0001_state.py");
        let db_migration = parse_migration(DB_ONLY_MIGRATION, "0002_db.py");
        let migrations = vec![&state_migration, &db_migration];

        let diagnostics = run(&migrations);

        assert_eq!(diagnostics.len(), 2);
        let paths: std::collections::HashSet<_> =
            diagnostics.iter().map(|d| d.path.clone()).collect();
        assert!(paths.contains(&Path::new("0001_state.py").to_path_buf()));
        assert!(paths.contains(&Path::new("0002_db.py").to_path_buf()));
    }

    #[test]
    fn test_single_state_migration_good() {
        let state_migration = parse_migration(STATE_ONLY_MIGRATION, "0001_state.py");
        let migrations = vec![&state_migration];

        let diagnostics = run(&migrations);

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_single_db_migration_good() {
        let db_migration = parse_migration(DB_ONLY_MIGRATION, "0001_db.py");
        let migrations = vec![&db_migration];

        let diagnostics = run(&migrations);

        assert!(diagnostics.is_empty());
    }
}
