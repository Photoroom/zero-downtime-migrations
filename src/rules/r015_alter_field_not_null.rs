//! R015: AlterField possibly changing to NOT NULL or a new type
//!
//! Detects `AlterField` operations whose new field is NOT NULL or whose
//! column type changes. The
//! diagnostic flags both genuine nullable→NOT NULL transitions and
//! benign re-stipulations of an already-NOT-NULL column (changing
//! `max_length`, `help_text`, etc., where the new field still happens
//! to be NOT NULL).
//!
//! ## Why the column scan
//!
//! `ALTER TABLE ... ALTER COLUMN col SET NOT NULL` normally scans
//! every row to verify no NULLs exist. The scan runs under an ACCESS
//! EXCLUSIVE lock that blocks reads and writes for its duration. The
//! one fast-path: Postgres skips the scan when a *validated*
//! `CHECK (col IS NOT NULL)` constraint already exists on the column,
//! because the constraint proves no NULL can be present. The
//! four-step pattern in the help builds that constraint without
//! blocking writes, then `SET NOT NULL` becomes a catalog-only
//! operation.
//!
//! ## Severity
//!
//! `Warning`, not `Error`: the rule cannot tell, from a single
//! `AlterField` operation, whether the column was previously nullable.
//! Without schema history, a NOT-NULL→NOT-NULL `AlterField` (very
//! common — e.g. widening a CharField) is indistinguishable from a
//! risky nullable→NOT-NULL transition. Treating every such operation
//! as `Error` blocks correct migrations; emitting a `Warning` flags
//! the operation for review without breaking CI.
//!
//! Use `# zdm: ignore R015` on the operation when you've verified the
//! column was already NOT NULL.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects AlterField changing to NOT NULL.
pub struct R015AlterFieldNotNull;

impl Rule for R015AlterFieldNotNull {
    fn id(&self) -> &'static str {
        "R015"
    }

    fn name(&self) -> &'static str {
        "alter-field-not-null"
    }

    fn description(&self) -> &'static str {
        "AlterField that ends up NOT NULL may scan every row, and a type change may \
         rewrite the table. Verify the column was already NOT NULL, or migrate via \
         a four-step pattern: NOT VALID CHECK, VALIDATE CONSTRAINT, SET NOT NULL, \
         DROP CONSTRAINT."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.database_effective_operations_of_type(OperationType::AlterField) {
            let OperationData::Field(data) = &op.data else {
                continue;
            };
            let Some(field) = &data.field else {
                continue;
            };
            if field.is_nullable && !field.is_type_change {
                continue;
            }
            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    if field.is_type_change {
                        format!(
                            "Changing '{}' column type may require a table rewrite",
                            data.field_name
                        )
                    } else if migration.framework.uses_sql_table_identity() {
                        format!(
                            "Changing '{}' to NOT NULL may require a full table scan",
                            data.field_name
                        )
                    } else {
                        format!(
                            "AlterField '{}' results in a NOT NULL column \
                             (may require a full table scan)",
                            data.field_name
                        )
                    },
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(if field.is_type_change {
                    "Use an additive column, backfill it, and switch application reads before removing the old column."
                } else {
                    include_str!("help/r015_alter_field_not_null.txt")
                }),
            );
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALTER_TO_NOT_NULL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AlterField(
            model_name='product',
            name='description',
            field=models.TextField(),
        ),
    ]
"#;

    const ALTER_NULLABLE_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AlterField(
            model_name='product',
            name='description',
            field=models.TextField(null=True),
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R015AlterFieldNotNull, source)
    }

    #[test]
    fn test_alter_to_not_null_bad() {
        let diagnostics = check_migration(ALTER_TO_NOT_NULL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R015");
    }

    #[test]
    fn test_alter_nullable_good() {
        let diagnostics = check_migration(ALTER_NULLABLE_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_r015_severity_is_warning() {
        // R015 cannot tell a genuine nullable → NOT NULL transition
        // from a benign AlterField on an already-NOT-NULL column.
        // Treating the diagnostic as Warning surfaces it for review
        // without breaking CI on the common false-positive form.
        let diagnostics = check_migration(ALTER_TO_NOT_NULL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Severity::Warning);
    }
}
