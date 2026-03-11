//! R015: AlterField changing to NOT NULL
//!
//! Detects AlterField operations that change a field from nullable to NOT NULL.
//! This operation scans all rows to validate no NULLs exist and can lock the table.
//!
//! ## Limitation
//!
//! This rule cannot determine whether the field was previously nullable. It flags
//! ALL AlterField operations where the resulting field is NOT NULL, which may produce
//! false positives when:
//! - The field was already NOT NULL (e.g., just changing max_length)
//! - The AlterField is only changing other properties (e.g., help_text)
//!
//! This is a fundamental limitation of static analysis without schema history.
//! To accurately detect nullable→NOT NULL transitions, the tool would need access
//! to the previous migration or the actual database schema. Consider using
//! `# zdm: ignore R015` inline comments for legitimate AlterField operations
//! that don't change nullability.

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
        "AlterField changing a field to NOT NULL requires scanning all rows to validate \
         no NULLs exist. Use a CHECK constraint with NOT VALID then VALIDATE separately."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::AlterField) {
            if let OperationData::Field(data) = &op.data {
                if let Some(ref field) = data.field {
                    // Check if the field is now NOT NULL
                    if !field.is_nullable {
                        diagnostics.push(Diagnostic {
                            rule_id: self.id(),
                            rule_name: self.name(),
                            message: format!(
                                "AlterField '{}' makes field NOT NULL",
                                data.field_name
                            ),
                            severity: self.severity(),
                            path: ctx.path.to_path_buf(),
                            span: op.span,
                            help: Some(
                                "Use a two-step approach: 1) Add a CHECK constraint with NOT VALID \
                                 to prevent new NULLs, 2) VALIDATE CONSTRAINT in a separate migration \
                                 to check existing rows without locking"
                                    .to_string(),
                            ),
                            fix: None,
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
}
