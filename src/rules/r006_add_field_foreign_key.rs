//! R006: AddField with ForeignKey on an existing model.
//!
//! Adding a ForeignKey via `AddField` against an existing table is doubly
//! expensive: it builds the index implicitly (acquiring a lock for the
//! duration) and validates the constraint against every existing row.
//!
//! This rule was originally split across R006 (FK locks the table) and
//! R007 (FK should be preceded by a concurrent index). They fired on the
//! same operation with overlapping prescriptions; R007 was retired and
//! its order-aware concurrent-index exemption is now part of R006.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects AddField with ForeignKey on an existing model.
pub struct R006AddFieldForeignKey;

impl Rule for R006AddFieldForeignKey {
    fn id(&self) -> &'static str {
        "R006"
    }

    fn name(&self) -> &'static str {
        "add-field-foreign-key"
    }

    fn description(&self) -> &'static str {
        "AddField with a ForeignKey on an existing table creates an implicit \
         index (locking the table while it builds) and validates the FK \
         constraint against every existing row. Pre-create the index with \
         AddIndexConcurrently earlier in the migration, or split the work via \
         SeparateDatabaseAndState."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        // Walk operations in order so the AddIndexConcurrently exemption is
        // only honored when the index appeared *before* the FK in this
        // migration — otherwise the FK would still acquire its lock before
        // the index exists.
        let mut diagnostics = Vec::new();
        let mut indexed_concurrently: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for op in &migration.operations {
            match op.op_type {
                OperationType::AddIndexConcurrently => {
                    if let OperationData::Index(idx) = &op.data {
                        indexed_concurrently.insert(idx.model_name.to_lowercase());
                    }
                }
                OperationType::AddField => {
                    let OperationData::Field(data) = &op.data else {
                        continue;
                    };
                    let Some(field) = &data.field else { continue };
                    if field.field_type != "ForeignKey" {
                        continue;
                    }

                    // Exempt if the model was just created in this migration.
                    if migration.is_model_created(&data.model_name) {
                        continue;
                    }
                    // Exempt if a concurrent index for the same model already
                    // appeared earlier in this migration.
                    if indexed_concurrently.contains(&data.model_name.to_lowercase()) {
                        continue;
                    }

                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: format!(
                            "AddField with ForeignKey on existing model '{}'",
                            data.model_name
                        ),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "Either pre-create the index with AddIndexConcurrently \
                             (in the same migration, *before* the AddField), or split \
                             the operation via SeparateDatabaseAndState: add the column \
                             without the FK constraint first, backfill, then add the \
                             constraint."
                                .to_string(),
                        ),
                        fix: None,
                    });
                }
                _ => {}
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
    use std::path::Path;

    const ADD_FK_EXISTING_MODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='order',
            name='product',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.product'),
        ),
    ]
"#;

    const ADD_FK_NEW_MODEL_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Order',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
            ],
        ),
        migrations.AddField(
            model_name='order',
            name='product',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.product'),
        ),
    ]
"#;

    const ADD_NON_FK_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='description',
            field=models.TextField(null=True),
        ),
    ]
"#;

    const ADD_FK_AFTER_CONCURRENT_INDEX_GOOD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['customer'], name='order_customer_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
    ]
"#;

    const ADD_FK_BEFORE_CONCURRENT_INDEX_BAD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.AddField(
            model_name='order',
            name='customer',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.customer'),
        ),
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['customer'], name='order_customer_idx'),
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
        R006AddFieldForeignKey.check(&migration, &ctx)
    }

    #[test]
    fn test_add_fk_existing_model_bad() {
        let diagnostics = check_migration(ADD_FK_EXISTING_MODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
        assert_eq!(diagnostics[0].severity, Severity::Error);
    }

    #[test]
    fn test_add_fk_new_model_good() {
        // FK on newly created model is exempt.
        let diagnostics = check_migration(ADD_FK_NEW_MODEL_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_add_non_fk_good() {
        let diagnostics = check_migration(ADD_NON_FK_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_add_fk_after_concurrent_index_good() {
        let diagnostics = check_migration(ADD_FK_AFTER_CONCURRENT_INDEX_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_add_fk_before_concurrent_index_bad() {
        // Pre-creating the concurrent index AFTER the AddField does not
        // protect against the lock; R006 must still fire.
        let diagnostics = check_migration(ADD_FK_BEFORE_CONCURRENT_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R006");
    }
}
