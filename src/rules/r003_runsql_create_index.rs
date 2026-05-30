//! R003: RunSQL with CREATE INDEX
//!
//! Detects RunSQL operations that contain CREATE INDEX without CONCURRENTLY.
//! This pattern bypasses Django's concurrent operations and can cause table locks.

use crate::ast::{
    any_sql_statement, sql_statement_contains_concurrently, sql_statement_contains_create_index,
    Migration, OperationData, OperationType,
};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects RunSQL with non-concurrent CREATE INDEX.
pub struct R003RunSQLCreateIndex;

impl Rule for R003RunSQLCreateIndex {
    fn id(&self) -> &'static str {
        "R003"
    }

    fn name(&self) -> &'static str {
        "runsql-create-index"
    }

    fn description(&self) -> &'static str {
        "RunSQL with CREATE INDEX (without CONCURRENTLY) takes an exclusive lock. \
         Use CREATE INDEX CONCURRENTLY instead."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.database_effective_operations_of_type(OperationType::RunSQL) {
            if let OperationData::RunSQL(data) = &op.data {
                // Check each statement independently: a non-concurrent
                // CREATE INDEX sharing a RunSQL with a concurrent one must
                // still fire (a whole-string check would see CONCURRENTLY
                // elsewhere and wrongly exempt it).
                let fires = any_sql_statement(&data.sql, |stmt| {
                    sql_statement_contains_create_index(stmt)
                        && !sql_statement_contains_concurrently(stmt)
                });
                if fires {
                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: "RunSQL contains CREATE INDEX without CONCURRENTLY".to_string(),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "Use CREATE INDEX CONCURRENTLY to avoid table locks. Each \
                             CREATE INDEX statement is checked independently — splitting \
                             a non-concurrent CREATE INDEX into the same RunSQL as a \
                             concurrent one does not exempt it."
                                .to_string(),
                        ),
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

    const RUNSQL_CREATE_INDEX_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX idx_name ON table_name (column);',
        ),
    ]
"#;

    const RUNSQL_CREATE_INDEX_CONCURRENT_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX CONCURRENTLY idx_name ON table_name (column);',
        ),
    ]
"#;

    const RUNSQL_OTHER_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='UPDATE table_name SET column = value;',
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R003RunSQLCreateIndex, source)
    }

    #[test]
    fn test_runsql_create_index_without_concurrently_is_flagged() {
        let diagnostics = check_migration(RUNSQL_CREATE_INDEX_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R003");
    }

    #[test]
    fn test_runsql_create_index_concurrent_good() {
        let diagnostics = check_migration(RUNSQL_CREATE_INDEX_CONCURRENT_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_runsql_other_good() {
        let diagnostics = check_migration(RUNSQL_OTHER_GOOD);
        assert!(diagnostics.is_empty());
    }

    const RUNSQL_CREATE_INDEX_IN_COMMENT_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='-- CREATE INDEX was discussed and rejected\nUPDATE t SET c = 1;',
        ),
    ]
"#;

    const RUNSQL_CREATE_INDEX_IN_STRING_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql="INSERT INTO audit_log (message) VALUES ('CREATE INDEX rolled out');",
        ),
    ]
"#;

    #[test]
    fn test_create_index_in_comment_does_not_fire() {
        // R003 used to substring-match the raw SQL and false-positive on
        // `-- CREATE INDEX` in a SQL comment. The fix routes through
        // `RunSQLOperation::contains_create_index`, which strips comments.
        let diagnostics = check_migration(RUNSQL_CREATE_INDEX_IN_COMMENT_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_create_index_in_string_literal_does_not_fire() {
        let diagnostics = check_migration(RUNSQL_CREATE_INDEX_IN_STRING_GOOD);
        assert!(diagnostics.is_empty());
    }

    const RUNSQL_CONCURRENTLY_ONLY_IN_COMMENT_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql="""-- We chose not to use CONCURRENTLY here
                CREATE INDEX foo ON t (c);""",
        ),
    ]
"#;

    #[test]
    fn test_concurrently_in_comment_does_not_hide_real_violation() {
        // The CREATE INDEX side already noise-strips via
        // `contains_create_index`. The CONCURRENTLY check must noise-strip
        // too, otherwise a comment mentioning CONCURRENTLY (here,
        // explaining why it wasn't used) hides a real non-concurrent
        // CREATE INDEX statement.
        let diagnostics = check_migration(RUNSQL_CONCURRENTLY_ONLY_IN_COMMENT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R003");
    }

    const RUNSQL_MIXED_CONCURRENT_AND_NON_CONCURRENT_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX a_idx ON t (a); CREATE INDEX CONCURRENTLY b_idx ON t (b);',
        ),
    ]
"#;

    #[test]
    fn test_mixed_concurrent_and_non_concurrent_statements_flags_the_non_concurrent_one() {
        // The whole-string `contains CREATE INDEX && !contains
        // CONCURRENTLY` check silently passed this case: CONCURRENTLY
        // appears somewhere in the SQL (on the second statement), so
        // the first statement's blocking lock escaped detection.
        // The fix walks statement-by-statement after splitting on
        // `;` so each CREATE INDEX is evaluated independently.
        let diagnostics = check_migration(RUNSQL_MIXED_CONCURRENT_AND_NON_CONCURRENT_BAD);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert_eq!(diagnostics[0].rule_id, "R003");
    }

    const RUNSQL_SEQUENCE_MIXED_CONCURRENT_AND_NON_CONCURRENT_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.RunSQL(
            sql=[
                'CREATE INDEX a_idx ON t (a)',
                'CREATE INDEX CONCURRENTLY b_idx ON t (b)',
            ],
        ),
    ]
"#;

    #[test]
    fn test_statement_sequence_preserves_boundaries_for_concurrently_check() {
        let diagnostics = check_migration(RUNSQL_SEQUENCE_MIXED_CONCURRENT_AND_NON_CONCURRENT_BAD);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert_eq!(diagnostics[0].rule_id, "R003");
    }

    const RUNSQL_PARAMETERIZED_SEQUENCE_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.RunSQL(
            sql=[
                ('CREATE INDEX a_idx ON t (a) WHERE tenant_id = %s', [1]),
            ],
        ),
    ]
"#;

    #[test]
    fn test_parameterized_statement_sequence_is_checked() {
        let diagnostics = check_migration(RUNSQL_PARAMETERIZED_SEQUENCE_BAD);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert_eq!(diagnostics[0].rule_id, "R003");
    }

    const RUNSQL_BOTH_CONCURRENT_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX CONCURRENTLY a_idx ON t (a); CREATE INDEX CONCURRENTLY b_idx ON t (b);',
        ),
    ]
"#;

    #[test]
    fn test_both_statements_concurrent_passes() {
        // Symmetry pin for the per-statement walk: when both
        // statements are concurrent, neither fires. Without this
        // pin, a too-aggressive split-and-check refactor could
        // false-positive on legitimate multi-CONCURRENTLY bundles.
        let diagnostics = check_migration(RUNSQL_BOTH_CONCURRENT_GOOD);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
    }
}
