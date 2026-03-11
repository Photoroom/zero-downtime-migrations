//! Rule definitions and implementations.
//!
//! Rules are organized into two categories:
//! - Per-file rules (R001-R007, R010-R017): Analyze individual migration files
//! - Changeset rules (R008-R009): Analyze sets of changed files in a PR
//!
//! Each rule implements either the `Rule` trait (per-file) or `ChangesetRule` trait.

mod r001_non_concurrent_add_index;
mod r002_unique_constraint_without_index;
mod r003_runsql_create_index;
mod r004_missing_atomic_false;
mod r005_remove_field_without_separate;
mod r006_add_field_foreign_key;
mod r007_fk_without_index;
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
pub use r007_fk_without_index::R007FKWithoutIndex;
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

    /// Whether this rule is enabled by default.
    fn enabled_by_default(&self) -> bool {
        true
    }
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
                Box::new(R007FKWithoutIndex),
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
        let registry = RuleRegistry::new();
        assert!(registry.rules().len() >= 15);
    }

    #[test]
    fn test_changeset_registry_has_all_rules() {
        let registry = ChangesetRuleRegistry::new();
        assert!(registry.rules().len() >= 2);
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
}
