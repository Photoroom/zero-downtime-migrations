//! R008: Disallowed file changes alongside migrations
//!
//! Detects when migrations are changed alongside other files that match
//! disallowed patterns. This is often a sign that database changes and
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
        "Migrations should not be changed alongside certain file types (e.g., .py files). \
         This ensures database changes are deployed separately from application code."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
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
            .disallowed_file_patterns
            .iter()
            .filter_map(|p| Pattern::new(p).ok())
            .collect();

        for file in other_changed_files {
            let file_name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");

            for pattern in &patterns {
                if pattern.matches(file_name) {
                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: format!(
                            "File '{}' matches disallowed pattern '{}' and changed alongside migrations",
                            file.display(),
                            pattern
                        ),
                        severity: self.severity(),
                        path: file.to_path_buf(),
                        span: Span::default(),
                        help: Some(
                            "Database migrations and application code should be deployed separately. \
                             Split this PR into separate changes."
                                .to_string(),
                        ),
                        fix: None,
                    });
                    break; // Only report once per file
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
    use std::path::PathBuf;

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
    fn test_py_file_alongside_migration_warns() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("app/models.py"), Path::new("app/views.py")];
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

        // Default pattern is *.py, so should warn
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].rule_id, "R008");
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
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

        // No migrations changed, so no warning
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_custom_patterns() {
        let migration = create_migration();
        let migrations = vec![&migration];
        let other_files = vec![Path::new("app/models.py"), Path::new("config.json")];
        let mut config = Config::default();
        config.disallowed_file_patterns = vec!["*.json".to_string()];
        let ctx = RuleContext {
            config: &config,
            path: Path::new("."),
        };

        let diagnostics = R008DisallowedFileChanges.check(&migrations, &other_files, &ctx);

        // Only *.json is disallowed now
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("config.json"));
    }
}
