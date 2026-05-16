//! R015: AlterField possibly changing to NOT NULL
//!
//! Detects `AlterField` operations whose new field is NOT NULL. The
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
        "AlterField on a field that ends up NOT NULL may scan every row to validate \
         the constraint. Verify the column was already NOT NULL, or migrate via \
         a four-step pattern: NOT VALID CHECK, VALIDATE CONSTRAINT, SET NOT NULL, \
         DROP CONSTRAINT."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::AlterField) {
            if let OperationData::Field(data) = &op.data {
                if let Some(ref field) = data.field {
                    if !field.is_nullable {
                        diagnostics.push(Diagnostic {
                            rule_id: self.id(),
                            rule_name: self.name(),
                            message: format!(
                                "AlterField '{}' results in a NOT NULL column \
                                 (may require a full table scan)",
                                data.field_name
                            ),
                            severity: self.severity(),
                            path: ctx.path.to_path_buf(),
                            span: op.span,
                            help: Some(
                                "If the column was already NOT NULL (e.g. you're only \
                                 changing max_length or help_text), this is safe — add \
                                 `# zdm: ignore R015` to suppress.\n\n\
                                 If this is a genuine nullable → NOT NULL transition, use \
                                 the four-step pattern. Pick a stable constraint name like \
                                 `<table>_<col>_not_null` so step 4 can DROP it precisely \
                                 (PostgreSQL ALTER TABLE ADD CONSTRAINT has no IF NOT EXISTS, \
                                 so partially-applied migrations need to be cleaned up before \
                                 re-running):\n  \
                                 1. ALTER TABLE ... ADD CONSTRAINT <table>_<col>_not_null \
                                 CHECK (col IS NOT NULL) NOT VALID;\n  \
                                 2. ALTER TABLE ... VALIDATE CONSTRAINT <table>_<col>_not_null;  \
                                 -- table-scan without blocking writes\n  \
                                 3. ALTER TABLE ... ALTER COLUMN col SET NOT NULL;  \
                                 -- PostgreSQL 12+: catalog-only, no scan, because the validated CHECK proves no NULLs. \
                                 On 11 and earlier, SET NOT NULL still scans the table, defeating the four-step pattern.\n  \
                                 4. ALTER TABLE ... DROP CONSTRAINT <table>_<col>_not_null;  \
                                 -- recommended cleanup; the CHECK is subsumed by NOT NULL, \
                                 but keeping both forces every INSERT to re-evaluate the predicate"
                                    .to_string(),
                            ),
                        });
                    }
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
    use std::path::Path;

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
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("test.py"),
        };
        R015AlterFieldNotNull.check(&migration, &ctx)
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
