//! R008: Disallowed file changes alongside migrations
//!
//! Detects when migrations are changed alongside other files that don't match
//! any allowed patterns. This is often a sign that database changes and
//! application code changes are too tightly coupled.

use std::path::Path;

use glob::Pattern;

use crate::ast::Migration;
use crate::config::Config;
use crate::diagnostics::{Diagnostic, Severity, Span};
use crate::rules::ChangesetRule;

/// Rule that detects disallowed file changes alongside migrations.
pub struct R008DisallowedFileChanges;

impl ChangesetRule for R008DisallowedFileChanges {
    fn id(&self) -> &'static str {
        "R008"
    }

    fn name(&self) -> &'static str {
        "disallowed-file-changes"
    }

    fn description(&self) -> &'static str {
        "Migrations should not be changed alongside certain file types. \
         Use allowed-file-patterns to specify which files may change alongside migrations. \
         By default only `models.py` is allowed, since makemigrations changes it together \
         with the migration it generates."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(
        &self,
        _migrations: &[&Migration],
        changed_migration_paths: &[&Path],
        other_changed_files: &[&Path],
        config: &Config,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // If no migrations are changed, nothing to check
        if changed_migration_paths.is_empty() {
            return diagnostics;
        }

        let patterns: Vec<Pattern> = config
            .allowed_file_patterns
            .iter()
            // Public Config fields can be mutated without going through the
            // loader. Invalid patterns fail closed instead of panicking or
            // accidentally allowing a changed file.
            .filter_map(|p| Pattern::new(p).ok())
            .collect();

        for file in other_changed_files {
            let is_allowed = !patterns.is_empty() && allowed_by_patterns(file, &patterns);
            if !is_allowed {
                diagnostics.push(
                    Diagnostic::new(
                        self.id(),
                        self.name(),
                        self.severity(),
                        format!(
                            "File '{}' does not match any allowed pattern and changed alongside migrations",
                            file.display(),
                        ),
                        file.to_path_buf(),
                        Span::default(),
                    )
                    .with_help(
                        "Database migrations and application code should be deployed separately. \
                         Split this PR into separate changes or add the pattern to allowed-file-patterns.",
                    ),
                );
            }
        }

        diagnostics
    }
}

/// Match a repo-relative path against the configured patterns.
///
/// Each pattern is tried against the full repo-relative path and against the
/// basename, so configs like `backend/**/models.py` and bare `models.py` both
/// work without the rule needing filesystem access.
fn allowed_by_patterns(file: &Path, patterns: &[Pattern]) -> bool {
    let path_str = file.to_string_lossy().replace('\\', "/");
    let file_name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");

    patterns
        .iter()
        .any(|pattern| pattern.matches(&path_str) || pattern.matches(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::MigrationExtractor;
    use crate::config::Config;
    use crate::parser::ParsedMigration;

    const SIMPLE_MIGRATION: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    operations = []
"#;

    fn create_migration() -> Migration {
        let parsed = ParsedMigration::parse(SIMPLE_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        extractor
            .extract(Path::new("app/migrations/0001.py"))
            .unwrap()
    }

    #[test]
    fn test_default_config_allows_models_py() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("app/models.py"), Path::new("app/views.py")];
        let config = Config::default();
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        // The default allowed-file-patterns covers models.py, so only
        // views.py is rejected.
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("views.py"));
    }

    #[test]
    fn test_explicit_empty_patterns_reject_all() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("app/models.py"), Path::new("app/views.py")];
        let config = Config {
            allowed_file_patterns: vec![],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        // Explicitly empty allowed patterns reject every non-migration file
        assert_eq!(diagnostics.len(), 2);
    }

    #[test]
    fn test_file_not_matching_allowed_pattern_warns() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("app/models.py"), Path::new("config.json")];
        let config = Config {
            allowed_file_patterns: vec!["*.json".to_string()],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        // *.py is not allowed, *.json is
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("models.py"));
    }

    #[test]
    fn test_all_files_matching_allowed_pattern_ok() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("config.json"), Path::new("data.json")];
        let config = Config {
            allowed_file_patterns: vec!["*.json".to_string()],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_no_other_files_good() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files: Vec<&Path> = vec![];
        let config = Config::default();
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_no_migrations_good() {
        let migrations: Vec<&Migration> = vec![];
        let other_files = vec![Path::new("app/models.py")];
        let config = Config {
            allowed_file_patterns: vec!["*.json".to_string()],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        // No migrations changed, so no warning
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_multiple_allowed_patterns() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![
            Path::new("config.json"),
            Path::new("data.yaml"),
            Path::new("app/models.py"),
        ];
        let config = Config {
            allowed_file_patterns: vec!["*.json".to_string(), "*.yaml".to_string()],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        // Only *.py should trigger
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("models.py"));
    }

    #[test]
    fn test_repo_relative_allowed_pattern_matches() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("backend/media/models.py")];
        let config = Config {
            allowed_file_patterns: vec!["backend/media/models.py".to_string()],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_repo_relative_glob_allowed_pattern_matches() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("backend/media/models.py")];
        let config = Config {
            allowed_file_patterns: vec!["backend/**/models.py".to_string()],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_basename_allowed_pattern_still_matches() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("backend/media/models.py")];
        let config = Config {
            allowed_file_patterns: vec!["models.py".to_string()],
            ..Default::default()
        };
        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_invalid_programmatic_pattern_fails_closed() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("app/models.py")];
        let config = Config {
            allowed_file_patterns: vec!["[".to_string()],
            ..Default::default()
        };

        let diagnostics = crate::rules::test_support::check_changeset_rule(
            &R008DisallowedFileChanges,
            &migrations,
            &other_files,
            &config,
        );

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("app/models.py"));
    }
}
