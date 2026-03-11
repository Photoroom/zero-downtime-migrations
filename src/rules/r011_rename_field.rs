//! R011: RenameField
//!
//! Detects RenameField operations which are inherently dangerous.
//! Renaming columns requires application changes to be deployed simultaneously.

use crate::ast::{Migration, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects RenameField operations.
pub struct R011RenameField;

impl Rule for R011RenameField {
    fn id(&self) -> &'static str {
        "R011"
    }

    fn name(&self) -> &'static str {
        "rename-field"
    }

    fn description(&self) -> &'static str {
        "RenameField is dangerous as it requires simultaneous deployment of application \
         code changes. Consider adding a new field, backfilling, then removing the old one."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::RenameField) {
            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: "RenameField requires simultaneous application deployment".to_string(),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: op.span,
                help: Some(
                    "Consider: 1) Add new field, 2) Copy data, 3) Update app to use new field, \
                     4) Remove old field. This allows gradual rollout."
                        .to_string(),
                ),
                fix: None,
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

    const RENAME_FIELD_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RenameField(
            model_name='product',
            old_name='old_name',
            new_name='new_name',
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
        R011RenameField.check(&migration, &ctx)
    }

    #[test]
    fn test_rename_field_warns() {
        let diagnostics = check_migration(RENAME_FIELD_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R011");
        assert_eq!(diagnostics[0].severity, Severity::Warning);
    }
}
