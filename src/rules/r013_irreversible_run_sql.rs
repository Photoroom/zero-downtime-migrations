//! R013: Irreversible RunSQL
//!
//! Detects RunSQL operations without reverse_sql.
//! Without reverse SQL, migrations cannot be rolled back.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects irreversible RunSQL operations.
pub struct R013IrreversibleRunSQL;

impl Rule for R013IrreversibleRunSQL {
    fn id(&self) -> &'static str {
        "R013"
    }

    fn name(&self) -> &'static str {
        "irreversible-run-sql"
    }

    fn description(&self) -> &'static str {
        "RunSQL without reverse_sql makes the migration irreversible. \
         Always provide reverse_sql or use migrations.RunSQL.noop."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::RunSQL) {
            if let OperationData::RunSQL(data) = &op.data {
                if data.reverse_sql.is_none() {
                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: "RunSQL has no reverse_sql".to_string(),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "Add reverse_sql parameter: RunSQL(sql, reverse_sql) or use \
                             RunSQL(sql, migrations.RunSQL.noop) if no reverse is needed"
                                .to_string(),
                        ),
                        fix: None,
                    });
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

    const IRREVERSIBLE_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='UPDATE table SET column = value;',
        ),
    ]
"#;

    const REVERSIBLE_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='UPDATE table SET column = value;',
            reverse_sql='UPDATE table SET column = old_value;',
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
        R013IrreversibleRunSQL.check(&migration, &ctx)
    }

    #[test]
    fn test_irreversible_run_sql_warns() {
        let diagnostics = check_migration(IRREVERSIBLE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R013");
    }

    #[test]
    fn test_reversible_run_sql_good() {
        let diagnostics = check_migration(REVERSIBLE_GOOD);
        assert!(diagnostics.is_empty());
    }
}
