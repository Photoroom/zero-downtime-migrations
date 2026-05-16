//! R005: RemoveField without SeparateDatabaseAndState
//!
//! Detects RemoveField operations not wrapped in SeparateDatabaseAndState.
//! Directly removing a field can cause errors if the application still
//! references the column.

use crate::ast::{Migration, ModelOperation, OperationData, OperationType};
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
        // R005's concern is *deployment* safety: a freshly-deployed app
        // still references the column, and a direct DROP COLUMN causes
        // missing-column errors on every read. That risk only applies
        // to columns the app already knows about.
        //
        // If a model is created earlier in the *same* migration, the
        // app couldn't possibly have referenced it before this migration
        // ran (the field never existed in any prior deployed schema),
        // so the SDaS wrap is overkill. Exempt those — same order-aware
        // walk shape as R001/R002/R006/R010/R016/R017.
        //
        // Note on extraction: top-level RemoveFields wrapped inside
        // SeparateDatabaseAndState are *not* surfaced as top-level ops
        // by the extractor (they live under
        // `OperationData::SeparateDatabaseAndState`), so any RemoveField
        // we see here is by definition not wrapped.
        let mut diagnostics = Vec::new();
        let mut created_so_far: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for op in &migration.operations {
            if op.op_type == OperationType::CreateModel {
                if let OperationData::Model(ModelOperation { name, .. }) = &op.data {
                    created_so_far.insert(name.to_lowercase());
                }
                continue;
            }
            if op.op_type != OperationType::RemoveField {
                continue;
            }
            if let OperationData::Field(data) = &op.data {
                if created_so_far.contains(&data.model_name.to_lowercase()) {
                    continue;
                }
            }

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

    const CREATEMODEL_BEFORE_REMOVEFIELD_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('legacy', models.CharField(max_length=50)),
            ],
        ),
        migrations.RemoveField(
            model_name='product',
            name='legacy',
        ),
    ]
"#;

    #[test]
    fn test_createmodel_before_removefield_is_exempted() {
        // A RemoveField on a model created earlier in the same migration
        // is safe: the app never had a chance to reference the column
        // (it never existed in any deployed schema), so the SDaS wrap
        // is overkill.
        let diagnostics = check_migration(CREATEMODEL_BEFORE_REMOVEFIELD_GOOD);
        assert!(
            diagnostics.is_empty(),
            "RemoveField on a freshly-created model should not fire R005, got: {diagnostics:?}",
        );
    }

    const REMOVEFIELD_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.RemoveField(
            model_name='product',
            name='legacy',
        ),
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
            ],
        ),
    ]
"#;

    #[test]
    fn test_removefield_before_createmodel_is_not_exempted() {
        // Symmetry pin for the order-aware fix above. A CreateModel
        // that runs *after* the RemoveField cannot retroactively
        // exempt — the RemoveField runs against whatever shape the
        // model had before the migration.
        let diagnostics = check_migration(REMOVEFIELD_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R005");
    }
}
