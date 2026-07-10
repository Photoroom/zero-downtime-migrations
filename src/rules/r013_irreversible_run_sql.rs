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

        for op in migration.database_effective_operations_of_type(OperationType::RunSQL) {
            let OperationData::RunSQL(data) = &op.data else {
                continue;
            };
            if data.reverse_sql.is_some() {
                continue;
            }
            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    "RunSQL has no reverse_sql",
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(
                    "Add reverse_sql parameter: RunSQL(sql, reverse_sql) or use \
                     RunSQL(sql, migrations.RunSQL.noop) if no reverse is needed",
                ),
            );
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    const NOOP_REVERSE_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='UPDATE table SET column = value;',
            reverse_sql=migrations.RunSQL.noop,
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R013IrreversibleRunSQL, source)
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

    #[test]
    fn test_runsql_noop_reverse_is_reversible() {
        let diagnostics = check_migration(NOOP_REVERSE_GOOD);
        assert!(diagnostics.is_empty());
    }

    const WRAPPED_IRREVERSIBLE_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RunSQL(
                    sql='UPDATE table SET column = value;',
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_irreversible_run_sql_warns() {
        // RunSQL wrapped in SeparateDatabaseAndState(database_operations=[...])
        // is database-effective and must still be flagged when irreversible.
        let diagnostics = check_migration(WRAPPED_IRREVERSIBLE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R013");
    }
}
