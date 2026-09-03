//! Rule definitions and implementations.
//!
//! Rules are organized into two categories:
//! - Per-file rules (R001-R006, R010-R018): Analyze individual migration files
//! - Changeset rules (R008-R009): Analyze sets of changed files in a PR
//!
//! R007 was merged into R006 and retired; the ID is intentionally skipped.
//!
//! Each rule implements either the `Rule` trait (per-file) or `ChangesetRule` trait.

#[cfg(test)]
pub(crate) mod test_support;

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
mod r018_implicit_django_index;

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
pub use r018_implicit_django_index::R018ImplicitDjangoIndex;

use std::collections::HashSet;
use std::path::Path;

use crate::ast::{
    Migration, ModelOperation, Operation, OperationData, OperationType, TableIdentity,
};
use crate::config::Config;
use crate::diagnostics::{Diagnostic, Severity};

/// Context passed to rules during linting.
///
/// `#[non_exhaustive]`: build via [`RuleContext::new`] so a
/// future field addition doesn't break out-of-tree rule authors.
#[non_exhaustive]
pub struct RuleContext<'a> {
    /// The configuration.
    pub config: &'a Config,
    /// The file path being linted.
    pub path: &'a Path,
}

impl<'a> RuleContext<'a> {
    /// Build a context for invoking a rule. Out-of-tree
    /// consumers building custom rules call this in their tests.
    pub fn new(config: &'a Config, path: &'a Path) -> Self {
        Self { config, path }
    }
}

/// Case-insensitive set of model names created so far during a
/// `walk_with_created_models` traversal. `contains` lowercases
/// on both sides so callers can pass `model_name` as-is.
pub struct CreatedModels {
    names: HashSet<String>,
    sql_tables: HashSet<TableIdentity>,
}

impl CreatedModels {
    fn new() -> Self {
        Self {
            names: HashSet::new(),
            sql_tables: HashSet::new(),
        }
    }

    fn insert(&mut self, name: &str) {
        self.names.insert(name.to_lowercase());
    }

    fn remove(&mut self, name: &str) {
        self.names.remove(&name.to_lowercase());
    }

    fn insert_sql_table(&mut self, table: TableIdentity) {
        self.sql_tables.insert(table);
    }

    fn clear_sql_tables(&mut self) {
        self.sql_tables.clear();
    }

    /// Checks the appropriate identity scheme for the operation's framework.
    pub fn contains_operation(&self, migration: &Migration, op: &Operation) -> bool {
        if migration.framework.uses_sql_table_identity() {
            return op
                .table_identity
                .as_ref()
                .is_some_and(|table| self.sql_tables.contains(table));
        }
        op.model_name().is_some_and(|name| self.contains(name))
    }

    /// Is `name` in the set? Comparison is case-insensitive on
    /// both sides — callers do not need to lowercase before
    /// calling.
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(&name.to_lowercase())
    }
}

/// Walk a migration's database-effective operations in source order,
/// threading created-model state through each callback.
///
/// Wrapped `database_operations` are visited at the wrapper's source
/// position. They inherit real top-level `CreateModel` operations that
/// came before the wrapper, but state-side operations inside the wrapper
/// remain metadata-only and do not mutate the created-model set.
pub(crate) fn walk_with_created_models(
    migration: &Migration,
    mut handle: impl FnMut(&Operation, &CreatedModels),
) {
    let mut created = CreatedModels::new();
    for op in &migration.operations {
        walk_database_effective_operation(migration, op, &mut created, &mut handle);
    }
}

fn walk_database_effective_operation(
    migration: &Migration,
    op: &Operation,
    created: &mut CreatedModels,
    handle: &mut impl FnMut(&Operation, &CreatedModels),
) {
    match &op.data {
        OperationData::SeparateDatabaseAndState(data) => {
            for db_op in &data.database_operations {
                walk_database_effective_operation(migration, db_op, created, handle);
            }
        }
        _ => {
            if migration.framework.uses_sql_table_identity()
                && op.op_type == OperationType::ExecuteSql
            {
                created.clear_sql_tables();
            }
            if let OperationData::Model(ModelOperation { name, old_name }) = &op.data {
                if op.op_type == OperationType::CreateModel {
                    if migration.framework.uses_sql_table_identity() {
                        if let Some(table) = op.table_identity.clone() {
                            created.insert_sql_table(table);
                        }
                    } else {
                        created.insert(name);
                    }
                } else if op.op_type == OperationType::RenameModel {
                    if let Some(old_name) = old_name {
                        created.remove(old_name);
                        created.insert(name);
                    }
                } else if op.op_type == OperationType::DeleteModel {
                    created.remove(name);
                }
            }
            handle(op, created);
        }
    }
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
    /// Changeset rules build their own diagnostic paths from the migration /
    /// file data, so they receive the `Config` directly rather than a
    /// per-file `RuleContext`.
    fn check(
        &self,
        migrations: &[&Migration],
        changed_migration_paths: &[&Path],
        other_changed_files: &[&Path],
        config: &Config,
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
                Box::new(R018ImplicitDjangoIndex),
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

    /// Append a custom rule. Out-of-tree consumers building
    /// custom rules call this after constructing the registry
    /// with [`Self::new`] (which seeds the built-ins). Rules
    /// run in registration order; suppression and
    /// `warnings_as_errors` apply uniformly.
    pub fn register(&mut self, rule: Box<dyn Rule>) {
        self.rules.push(rule);
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

            apply_warnings_as_errors(&mut rule_diagnostics, config);
            diagnostics.extend(rule_diagnostics);
        }

        diagnostics
    }
}

/// Promote every `Warning` to `Error` if `config.warnings_as_errors`
/// is set. Shared post-processing between `RuleRegistry::check` and
/// `ChangesetRuleRegistry::check` — both registries used to have a
/// hand-rolled copy of the same loop, and a future severity tweak
/// would have needed two edits.
fn apply_warnings_as_errors(diagnostics: &mut [Diagnostic], config: &Config) {
    if !config.warnings_as_errors {
        return;
    }
    for diag in diagnostics {
        if diag.severity == Severity::Warning {
            diag.severity = Severity::Error;
        }
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

    /// Append a custom changeset rule. See [`RuleRegistry::register`].
    pub fn register(&mut self, rule: Box<dyn ChangesetRule>) {
        self.rules.push(rule);
    }

    /// Run all changeset rules.
    pub fn check(
        &self,
        migrations: &[&Migration],
        changed_migration_paths: &[&Path],
        other_changed_files: &[&Path],
        config: &Config,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for rule in &self.rules {
            if config.is_rule_enabled(rule.id()) {
                let mut rule_diagnostics = rule.check(
                    migrations,
                    changed_migration_paths,
                    other_changed_files,
                    config,
                );

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

                apply_warnings_as_errors(&mut rule_diagnostics, config);
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
                "R014", "R015", "R016", "R017", "R018",
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
        use crate::ast::MigrationExtractor;
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
        let migration_paths: Vec<&Path> = migrations.iter().map(|m| m.path.as_path()).collect();
        let other_files: Vec<&Path> = vec![];
        let diagnostics = registry.check(&migrations, &migration_paths, &other_files, &config);

        // R009 emits one diagnostic per file in the pair; the state-side
        // suppresses, so only the db-side diagnostic survives.
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R009");
        assert_eq!(diagnostics[0].path, Path::new("0002_db.py"));
    }

    #[test]
    fn test_changeset_registry_suppresses_r008_via_any_migration() {
        use crate::ast::MigrationExtractor;
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
        let migration_paths = vec![migration.path.as_path()];
        // The non-migration changed file would normally trigger R008
        // (no allowed-file-patterns configured, so all non-migration
        // files are flagged).
        let models = Path::new("app/models.py");
        let other_files: Vec<&Path> = vec![models];
        let diagnostics = registry.check(&migrations, &migration_paths, &other_files, &config);

        assert!(
            diagnostics.is_empty(),
            "R008 should be suppressed by `# zdm: ignore R008` in the migration; got {diagnostics:?}",
        );
    }

    #[test]
    fn test_changeset_registry_does_not_suppress_r008_via_unrelated_rule() {
        use crate::ast::MigrationExtractor;
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
        let migration_paths = vec![migration.path.as_path()];
        let models = Path::new("app/models.py");
        let other_files: Vec<&Path> = vec![models];
        let diagnostics = registry.check(&migrations, &migration_paths, &other_files, &config);

        // R008 still fires because the directive targets a different rule.
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R008");
    }

    // -------------------------------------------------------------------
    // walk_with_created_models
    //
    // The helper consolidates a pattern that used to be hand-rolled in
    // R001/R002/R006/R010/R016/R017. Tests pin the invariants the
    // callers rely on so a future refactor of the helper can't
    // silently change exemption behaviour for all six rules at once.
    // -------------------------------------------------------------------

    use std::path::Path;

    fn extract(source: &str) -> Migration {
        Migration::from_source(Path::new("test.py"), source).unwrap()
    }

    #[test]
    fn walk_with_created_models_strictly_before() {
        // A CreateModel reaches `created_so_far` for the callbacks
        // of every op that follows it, but not the op before it.
        // This is the invariant that makes the order-aware
        // exemption work — an order-blind set would silently let a
        // CreateModel below a flagged op exempt that op.
        let source = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
        migrations.CreateModel(
            name='Product',
            fields=[('id', models.BigAutoField(primary_key=True))],
        ),
    ]
"#;
        let migration = extract(source);
        let mut seen: Vec<(OperationType, bool)> = Vec::new();
        walk_with_created_models(&migration, |op, created| {
            seen.push((op.op_type, created.contains("product")));
        });
        assert_eq!(
            seen,
            vec![
                (OperationType::AddIndex, false),
                (OperationType::CreateModel, true),
            ],
            "AddIndex above its CreateModel must NOT see the model in created",
        );
    }

    #[test]
    fn walk_with_created_models_case_insensitive_both_sides() {
        // The CreatedModels newtype lowercases on insert AND on
        // lookup, so a caller passing the original title-cased
        // name finds the model. Before the newtype existed,
        // callers had to remember to lowercase on lookup too;
        // forgetting silently missed exemptions. Pin both halves.
        let source = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[('id', models.BigAutoField(primary_key=True))],
        ),
    ]
"#;
        let migration = extract(source);
        let mut saw_lowercase = false;
        let mut saw_titlecase = false;
        let mut saw_uppercase = false;
        walk_with_created_models(&migration, |_, created| {
            saw_lowercase |= created.contains("product");
            saw_titlecase |= created.contains("Product");
            saw_uppercase |= created.contains("PRODUCT");
        });
        assert!(saw_lowercase && saw_titlecase && saw_uppercase);
    }

    #[test]
    fn walk_with_created_models_tracks_renames() {
        let migration = extract(
            r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[('id', models.BigAutoField(primary_key=True))],
        ),
        migrations.RenameModel(old_name='Product', new_name='Item'),
        migrations.AddIndex(
            model_name='item',
            index=models.Index(fields=['name'], name='item_name_idx'),
        ),
    ]
"#,
        );
        let mut state = Vec::new();
        walk_with_created_models(&migration, |op, created| {
            state.push((
                op.op_type,
                created.contains("product"),
                created.contains("item"),
            ));
        });
        assert_eq!(
            state,
            [
                (OperationType::CreateModel, true, false),
                (OperationType::RenameModel, false, true),
                (OperationType::AddIndex, false, true),
            ]
        );
    }

    #[test]
    fn walk_with_created_models_visits_wrapped_database_ops_in_source_order() {
        let source = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[('id', models.BigAutoField(primary_key=True))],
        ),
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.AddIndex(
                    model_name='product',
                    index=models.Index(fields=['name'], name='product_name_idx'),
                ),
            ],
            state_operations=[
                migrations.CreateModel(
                    name='StateOnly',
                    fields=[('id', models.BigAutoField(primary_key=True))],
                ),
            ],
        ),
        migrations.AddIndex(
            model_name='stateonly',
            index=models.Index(fields=['name'], name='stateonly_name_idx'),
        ),
    ]
"#;
        let migration = extract(source);
        let mut seen: Vec<(OperationType, bool, bool)> = Vec::new();
        walk_with_created_models(&migration, |op, created| {
            seen.push((
                op.op_type,
                created.contains("product"),
                created.contains("stateonly"),
            ));
        });
        assert_eq!(
            seen,
            vec![
                (OperationType::CreateModel, true, false),
                (OperationType::AddIndex, true, false),
                (OperationType::AddIndex, true, false),
            ],
            "wrapped database ops should inherit prior real CreateModel state, \
             and state_operations must not create database state",
        );
    }

    #[test]
    fn walk_with_created_models_non_createmodel_does_not_insert() {
        // The set must be populated *only* by CreateModel ops.
        // A future helper tweak that also recorded e.g.
        // AddField target models would silently expand exemptions
        // for every caller and break their semantics. Pin the
        // invariant.
        let source = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;
        let migration = extract(source);
        let mut saw_order = false;
        walk_with_created_models(&migration, |_, created| {
            saw_order |= created.contains("order");
        });
        assert!(
            !saw_order,
            "non-CreateModel ops must not populate `created`",
        );
    }
}
