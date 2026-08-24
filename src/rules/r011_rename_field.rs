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
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.database_effective_operations_of_type(OperationType::RenameField) {
            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    if migration.framework.uses_sql_table_identity() {
                        "Renaming a column requires simultaneous application deployment"
                    } else {
                        "RenameField requires simultaneous application deployment"
                    },
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(
                    "Consider: 1) Add new field, 2) Copy data, 3) Update app to use new field, \
                     4) Remove old field. This allows gradual rollout.",
                ),
            );
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        crate::rules::test_support::check_rule(&R011RenameField, source)
    }

    const WRAPPED_RENAME_FIELD_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RenameField(
                    model_name='product',
                    old_name='old_name',
                    new_name='new_name',
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_renamefield_is_flagged() {
        let diagnostics = check_migration(RENAME_FIELD_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R011");
        assert_eq!(diagnostics[0].severity, Severity::Error);
    }

    #[test]
    fn test_wrapped_renamefield_is_flagged() {
        // A RenameField inside SeparateDatabaseAndState(database_operations=[...])
        // is still a database-effective rename, consistent with R010/R016/R017.
        let diagnostics = check_migration(WRAPPED_RENAME_FIELD_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R011");
    }
}
