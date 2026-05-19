//! R008: Disallowed file changes alongside migrations
//!
//! Detects when migrations are changed alongside other files that don't match
//! any allowed patterns. This is often a sign that database changes and
//! application code changes are too tightly coupled.

use std::path::Path;

use glob::Pattern;

use crate::ast::Migration;
use crate::diagnostics::{Diagnostic, Severity, Span};
use crate::rules::{ChangesetRule, RuleContext};

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
         Use allowed-file-patterns to specify which files may change alongside migrations."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(
        &self,
        migrations: &[&Migration],
        other_changed_files: &[&Path],
        ctx: &RuleContext,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // If no migrations are changed, nothing to check
        if migrations.is_empty() {
            return diagnostics;
        }

        let patterns: Vec<Pattern> = ctx
            .config
            .allowed_file_patterns
            .iter()
            .map(|p| Pattern::new(p).expect("allowed_file_patterns are validated at config load"))
            .collect();

        for file in other_changed_files {
            let is_allowed = !patterns.is_empty() && allowed_by_patterns(file, &patterns);
            if !is_allowed {
                diagnostics.push(Diagnostic {
                    rule_id: self.id(),
                    rule_name: self.name(),
                    message: format!(
                        "File '{}' does not match any allowed pattern and changed alongside migrations",
                        file.display(),
                    ),
                    severity: self.severity(),
                    path: file.to_path_buf(),
                    span: Span::default(),
                    help: Some(
                        "Database migrations and application code should be deployed separately. \
                         Split this PR into separate changes or add the pattern to allowed-file-patterns."
                            .to_string(),
                    ),
                });
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
    fn test_no_allowed_patterns_rejects_all() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("app/models.py"), Path::new("app/views.py")];
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

        // No allowed patterns configured, all non-migration files are rejected
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
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

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
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_no_other_files_good() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files: Vec<&Path> = vec![];
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

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
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

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
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

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
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

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
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

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
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

        assert!(diagnostics.is_empty());
    }
}
