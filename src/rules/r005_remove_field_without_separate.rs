//! R005: RemoveField without SeparateDatabaseAndState
//!
//! Detects RemoveField operations not wrapped in SeparateDatabaseAndState.
//! Directly removing a field can cause errors if the application still
//! references the column.

use crate::ast::{Migration, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects RemoveField without SeparateDatabaseAndState.
pub struct R005RemoveFieldWithoutSeparate;

impl Rule for R005RemoveFieldWithoutSeparate {
    fn id(&self) -> &'static str {
        "R005"
    }

    fn name(&self) -> &'static str {
        "remove-field-without-separate"
    }

    fn description(&self) -> &'static str {
        "RemoveField should be wrapped in SeparateDatabaseAndState to separate \
         the schema change from Django's state. First remove from Django state, \
         deploy, then drop the column in a separate migration."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Note: We only extract top-level operations. If a RemoveField is properly
        // wrapped inside SeparateDatabaseAndState, it won't be extracted as a
        // separate operation. Therefore, any RemoveField we see here is NOT wrapped
        // and should be flagged.

        for op in migration.operations_of_type(OperationType::RemoveField) {
            diagnostics.push(Diagnostic {
                rule_id: self.id(),
                rule_name: self.name(),
                message: "RemoveField without SeparateDatabaseAndState can cause errors"
                    .to_string(),
                severity: self.severity(),
                path: ctx.path.to_path_buf(),
                span: op.span,
                help: Some(
                    "Wrap RemoveField in SeparateDatabaseAndState. First migration removes \
                     from state (state_operations), deploy the app, then second migration \
                     drops the column (database_operations)."
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

    const REMOVE_FIELD_DIRECT_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RemoveField(
            model_name='product',
            name='deprecated_field',
        ),
    ]
"#;

    const REMOVE_FIELD_WITH_SEPARATE_GOOD: &str = r#"
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

    // This tests the short-circuit bug fix: having SeparateDatabaseAndState
    // should NOT exempt a direct RemoveField at the top level
    const MIXED_SEPARATE_AND_DIRECT_REMOVE_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            state_operations=[
                migrations.RemoveField(
                    model_name='product',
                    name='old_field',
                ),
            ],
        ),
        migrations.RemoveField(
            model_name='product',
            name='another_field',
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
        R005RemoveFieldWithoutSeparate.check(&migration, &ctx)
    }

    #[test]
    fn test_remove_field_direct_bad() {
        let diagnostics = check_migration(REMOVE_FIELD_DIRECT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R005");
    }

    #[test]
    fn test_remove_field_with_separate_good() {
        let diagnostics = check_migration(REMOVE_FIELD_WITH_SEPARATE_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_mixed_separate_and_direct_remove_flags_direct() {
        // Having SeparateDatabaseAndState should NOT exempt a direct RemoveField
        let diagnostics = check_migration(MIXED_SEPARATE_AND_DIRECT_REMOVE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R005");
    }
}
