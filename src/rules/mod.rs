//! Rule definitions and implementations.
//!
//! Rules are organized into two categories:
//! - Per-file rules (R001-R006, R010-R017): Analyze individual migration files
//! - Changeset rules (R008-R009): Analyze sets of changed files in a PR
//!
//! R007 was merged into R006 and retired; the ID is intentionally skipped.
//!
//! Each rule implements either the `Rule` trait (per-file) or `ChangesetRule` trait.

mod r001_non_concurrent_add_index;
mod r002_unique_constraint_without_index;
mod r003_runsql_create_index;
mod r004_missing_atomic_false;
mod r005_remove_field_without_separate;
mod r006_add_field_foreign_key;
mod r008_disallowed_file_changes;
mod r009_separate_db_state_same_pr;
mod r010_add_field_not_null;
mod r011_rename_field;
mod r012_irreversible_run_python;
mod r013_irreversible_run_sql;
mod r014_model_imports;
mod r015_alter_field_not_null;
mod r016_non_concurrent_remove_index;
mod r017_non_concurrent_add_constraint;

pub use r001_non_concurrent_add_index::R001NonConcurrentAddIndex;
pub use r002_unique_constraint_without_index::R002UniqueConstraintWithoutIndex;
pub use r003_runsql_create_index::R003RunSQLCreateIndex;
pub use r004_missing_atomic_false::R004MissingAtomicFalse;
pub use r005_remove_field_without_separate::R005RemoveFieldWithoutSeparate;
pub use r006_add_field_foreign_key::R006AddFieldForeignKey;
pub use r008_disallowed_file_changes::R008DisallowedFileChanges;
pub use r009_separate_db_state_same_pr::R009SeparateDbStateSamePr;
pub use r010_add_field_not_null::R010AddFieldNotNull;
pub use r011_rename_field::R011RenameField;
pub use r012_irreversible_run_python::R012IrreversibleRunPython;
pub use r013_irreversible_run_sql::R013IrreversibleRunSQL;
pub use r014_model_imports::R014ModelImports;
pub use r015_alter_field_not_null::R015AlterFieldNotNull;
pub use r016_non_concurrent_remove_index::R016NonConcurrentRemoveIndex;
pub use r017_non_concurrent_add_constraint::R017NonConcurrentAddConstraint;

use std::path::Path;

use crate::ast::Migration;
use crate::config::Config;
use crate::diagnostics::{Diagnostic, Severity};

/// Context passed to rules during linting.
pub struct RuleContext<'a> {
    /// The configuration.
    pub config: &'a Config,
    /// The file path being linted.
    pub path: &'a Path,
}

/// A per-file rule that analyzes individual migration files.
pub trait Rule: Send + Sync {
    /// The unique rule identifier (e.g., "R001").
    fn id(&self) -> &'static str;

    /// A short description of what the rule checks.
    fn name(&self) -> &'static str;

    /// A detailed explanation of the rule.
    fn description(&self) -> &'static str;

    /// The default severity level.
    fn severity(&self) -> Severity;

    /// Run the rule on a migration file.
    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic>;
}

/// A changeset rule that analyzes sets of changed files together.
pub trait ChangesetRule: Send + Sync {
    /// The unique rule identifier (e.g., "R008").
    fn id(&self) -> &'static str;

    /// A short description of what the rule checks.
    fn name(&self) -> &'static str;

    /// A detailed explanation of the rule.
    fn description(&self) -> &'static str;

    /// The default severity level.
    fn severity(&self) -> Severity;

    /// Run the rule on a set of changed migrations and other changed files.
    fn check(
        &self,
        migrations: &[&Migration],
        other_changed_files: &[&Path],
        ctx: &RuleContext,
    ) -> Vec<Diagnostic>;
}

/// Registry of all available rules.
pub struct RuleRegistry {
    rules: Vec<Box<dyn Rule>>,
}

impl Default for RuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleRegistry {
    /// Create a new registry with all built-in rules.
    pub fn new() -> Self {
        Self {
            rules: vec![
                Box::new(R001NonConcurrentAddIndex),
                Box::new(R002UniqueConstraintWithoutIndex),
                Box::new(R003RunSQLCreateIndex),
                Box::new(R004MissingAtomicFalse),
                Box::new(R005RemoveFieldWithoutSeparate),
                Box::new(R006AddFieldForeignKey),
                Box::new(R010AddFieldNotNull),
                Box::new(R011RenameField),
                Box::new(R012IrreversibleRunPython),
                Box::new(R013IrreversibleRunSQL),
                Box::new(R014ModelImports),
                Box::new(R015AlterFieldNotNull),
                Box::new(R016NonConcurrentRemoveIndex),
                Box::new(R017NonConcurrentAddConstraint),
            ],
        }
    }

    /// Get all rules.
    pub fn rules(&self) -> &[Box<dyn Rule>] {
        &self.rules
    }

    /// Get a rule by ID.
    pub fn get(&self, id: &str) -> Option<&dyn Rule> {
        self.rules.iter().find(|r| r.id() == id).map(|r| r.as_ref())
    }

    /// Get enabled rules based on config.
    pub fn enabled_rules(&self, config: &Config) -> Vec<&dyn Rule> {
        self.rules
            .iter()
            .filter(|r| config.is_rule_enabled(r.id()))
            .map(|r| r.as_ref())
            .collect()
    }

    /// Run all enabled rules on a migration.
    pub fn check(&self, migration: &Migration, config: &Config) -> Vec<Diagnostic> {
        let ctx = RuleContext {
            config,
            path: &migration.path,
        };

        let mut diagnostics = Vec::new();

        for rule in self.enabled_rules(config) {
            let mut rule_diagnostics = rule.check(migration, &ctx);

            // Drop diagnostics suppressed by a `# zdm: ignore RXXX`
            // comment on (or just above) the diagnostic's span.
            rule_diagnostics.retain(|d| {
                !migration.is_rule_suppressed_at(d.rule_id, d.span.start_line, d.span.end_line)
            });

            // Apply warnings_as_errors
            if config.warnings_as_errors {
                for diag in &mut rule_diagnostics {
                    if diag.severity == Severity::Warning {
                        diag.severity = Severity::Error;
                    }
                }
            }

            diagnostics.extend(rule_diagnostics);
        }

        diagnostics
    }
}

/// Registry of all changeset rules.
pub struct ChangesetRuleRegistry {
    rules: Vec<Box<dyn ChangesetRule>>,
}

impl Default for ChangesetRuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangesetRuleRegistry {
    /// Create a new registry with all built-in changeset rules.
    pub fn new() -> Self {
        Self {
            rules: vec![
                Box::new(R008DisallowedFileChanges),
                Box::new(R009SeparateDbStateSamePr),
            ],
        }
    }

    /// Get all rules.
    pub fn rules(&self) -> &[Box<dyn ChangesetRule>] {
        &self.rules
    }

    /// Get a rule by ID.
    pub fn get(&self, id: &str) -> Option<&dyn ChangesetRule> {
        self.rules.iter().find(|r| r.id() == id).map(|r| r.as_ref())
    }

    /// Run all changeset rules.
    pub fn check(
        &self,
        migrations: &[&Migration],
        other_changed_files: &[&Path],
        config: &Config,
    ) -> Vec<Diagnostic> {
        let ctx = RuleContext {
            config,
            path: Path::new("."),
        };

        let mut diagnostics = Vec::new();

        for rule in &self.rules {
            if config.is_rule_enabled(rule.id()) {
                let mut rule_diagnostics = rule.check(migrations, other_changed_files, &ctx);

                // Honour `# zdm: ignore RXXX` comments. Two cases:
                //
                //   1. The diagnostic's path matches one of the changeset's
                //      migrations (R009 — fires on a migration file).
                //      Suppression is per-file, anchored by the diagnostic's
                //      span like the per-file rule registry.
                //
                //   2. The diagnostic's path is a non-migration file (R008
                //      — fires on `app/models.py`, `config/...`). The user
                //      cannot put a directive in that file; the only place
                //      they can write the comment is in a migration that
                //      is part of the same changeset. Treat a
                //      `# zdm: ignore RXXX` anywhere in any of the
                //      changeset's migrations as a changeset-wide
                //      suppression for that rule.
                rule_diagnostics.retain(|d| {
                    if let Some(migration) = migrations.iter().find(|m| m.path == d.path) {
                        !migration.is_rule_suppressed_at(
                            d.rule_id,
                            d.span.start_line,
                            d.span.end_line,
                        )
                    } else {
                        !migrations
                            .iter()
                            .any(|m| m.line_ignores.values().any(|ids| ids.contains(d.rule_id)))
                    }
                });

                // Apply warnings_as_errors
                if config.warnings_as_errors {
                    for diag in &mut rule_diagnostics {
                        if diag.severity == Severity::Warning {
                            diag.severity = Severity::Error;
                        }
                    }
                }

                diagnostics.extend(rule_diagnostics);
            }
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_has_all_rules() {
        // R007 was merged into R006 and retired; its ID is intentionally
        // absent and must not be reused for a new rule.
        let registry = RuleRegistry::new();
        let ids: Vec<&str> = registry.rules().iter().map(|r| r.id()).collect();
        assert_eq!(
            ids,
            vec![
                "R001", "R002", "R003", "R004", "R005", "R006", "R010", "R011", "R012", "R013",
                "R014", "R015", "R016", "R017",
            ],
        );
    }

    #[test]
    fn test_changeset_registry_has_all_rules() {
        let registry = ChangesetRuleRegistry::new();
        let ids: Vec<&str> = registry.rules().iter().map(|r| r.id()).collect();
        assert_eq!(ids, vec!["R008", "R009"]);
    }

    #[test]
    fn test_get_rule_by_id() {
        let registry = RuleRegistry::new();
        assert!(registry.get("R001").is_some());
        assert!(registry.get("R999").is_none());
    }

    #[test]
    fn test_enabled_rules_with_select() {
        let registry = RuleRegistry::new();
        let mut config = Config::default();
        config.select.insert("R001".to_string());

        let enabled = registry.enabled_rules(&config);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id(), "R001");
    }

    #[test]
    fn test_enabled_rules_with_ignore() {
        let registry = RuleRegistry::new();
        let mut config = Config::default();
        config.ignore.insert("R001".to_string());

        let enabled = registry.enabled_rules(&config);
        assert!(enabled.iter().all(|r| r.id() != "R001"));
    }

    #[test]
    fn test_changeset_registry_honours_inline_suppression() {
        use crate::ast::extractor::MigrationExtractor;
        use crate::parser::ParsedMigration;
        use std::path::Path;

        // Two SeparateDatabaseAndState halves in the same changeset would
        // normally fire R009 twice. Putting `# zdm: ignore R009` on the
        // state-half migration must suppress R009 for that file.
        const STATE: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        # zdm: ignore R009
        migrations.SeparateDatabaseAndState(
            state_operations=[
                migrations.RemoveField(model_name='p', name='f'),
            ],
        ),
    ]
"#;
        const DB: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RunSQL('DROP COLUMN f'),
            ],
        ),
    ]
"#;
        let parse = |src: &str, path: &str| -> Migration {
            let parsed = ParsedMigration::parse(src).unwrap();
            let extractor = MigrationExtractor::new(&parsed);
            extractor.extract(Path::new(path)).unwrap()
        };
        let state_m = parse(STATE, "0001_state.py");
        let db_m = parse(DB, "0002_db.py");

        let registry = ChangesetRuleRegistry::new();
        let config = Config::default();
        let migrations = vec![&state_m, &db_m];
        let other_files: Vec<&Path> = vec![];
        let diagnostics = registry.check(&migrations, &other_files, &config);

        // R009 emits one diagnostic per file in the pair; the state-side
        // suppresses, so only the db-side diagnostic survives.
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R009");
        assert_eq!(diagnostics[0].path, Path::new("0002_db.py"));
    }

    #[test]
    fn test_changeset_registry_suppresses_r008_via_any_migration() {
        use crate::ast::extractor::MigrationExtractor;
        use crate::parser::ParsedMigration;
        use std::path::Path;

        // R008's diagnostic fires on the non-migration changed file
        // (`models.py`), not on a migration. The user can't put a
        // `# zdm: ignore R008` directive in `models.py` because the
        // suppression machinery only inspects migration files. So a
        // `# zdm: ignore R008` placed anywhere in any of the changeset's
        // migrations counts as a changeset-wide opt-out for that rule.
        const STATE: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    # zdm: ignore R008
    operations = []
"#;
        let parsed = ParsedMigration::parse(STATE).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("0001.py")).unwrap();

        let registry = ChangesetRuleRegistry::new();
        let config = Config::default();
        let migrations = vec![&migration];
        // The non-migration changed file would normally trigger R008
        // (no allowed-file-patterns configured, so all non-migration
        // files are flagged).
        let models = Path::new("app/models.py");
        let other_files: Vec<&Path> = vec![models];
        let diagnostics = registry.check(&migrations, &other_files, &config);

        assert!(
            diagnostics.is_empty(),
            "R008 should be suppressed by `# zdm: ignore R008` in the migration; got {diagnostics:?}",
        );
    }

    #[test]
    fn test_changeset_registry_does_not_suppress_r008_via_unrelated_rule() {
        use crate::ast::extractor::MigrationExtractor;
        use crate::parser::ParsedMigration;
        use std::path::Path;

        // Sibling of `test_changeset_registry_suppresses_r008_via_any_migration`:
        // a `# zdm: ignore R009` directive in a migration must not
        // collateral-suppress R008. The changeset-wide branch is keyed
        // by rule_id, not "any directive present".
        const MIGRATION: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    # zdm: ignore R009
    operations = []
"#;
        let parsed = ParsedMigration::parse(MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("0001.py")).unwrap();

        let registry = ChangesetRuleRegistry::new();
        let config = Config::default();
        let migrations = vec![&migration];
        let models = Path::new("app/models.py");
        let other_files: Vec<&Path> = vec![models];
        let diagnostics = registry.check(&migrations, &other_files, &config);

        // R008 still fires because the directive targets a different rule.
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R008");
    }
}
