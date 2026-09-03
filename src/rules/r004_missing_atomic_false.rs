//! R004: Missing atomic=False for concurrent operations
//!
//! Concurrent index operations (AddIndexConcurrently, RemoveIndexConcurrently)
//! cannot run inside a transaction. The migration must have `atomic = False`.

use crate::ast::{
    any_sql_statement, sql_statement_contains_concurrently, sql_statement_contains_create_index,
    sql_statement_contains_drop_index, sql_statement_contains_reindex, Migration, Operation,
    OperationData, OperationType,
};
use crate::diagnostics::{Diagnostic, Severity};
use crate::discovery::MigrationFramework;
use crate::rules::{Rule, RuleContext};

/// Rule that detects concurrent operations without atomic=False.
pub struct R004MissingAtomicFalse;

impl Rule for R004MissingAtomicFalse {
    fn id(&self) -> &'static str {
        "R004"
    }

    fn name(&self) -> &'static str {
        "missing-atomic-false"
    }

    fn description(&self) -> &'static str {
        "Concurrent index operations cannot run inside a transaction. \
         Add `atomic = False` to the Migration class."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        if migration.framework == MigrationFramework::Aerich {
            return migration
                .database_effective_operations()
                .filter(|op| {
                    operation_requires_non_atomic(op)
                        && (!migration.is_non_atomic || op.in_autocommit_block)
                })
                .map(|op| {
                    Diagnostic::new(
                        self.id(),
                        self.name(),
                        self.severity(),
                        "Aerich concurrent operation may run inside a transaction",
                        ctx.path.to_path_buf(),
                        op.span,
                    )
                    .with_help(
                        "Use `RUN_IN_TRANSACTION = False` in a generated migration; concurrent SQL must also be the script's only statement.",
                    )
                })
                .collect();
        }
        if migration.framework == MigrationFramework::Alembic {
            return migration
                .database_effective_operations()
                .filter(|op| operation_requires_non_atomic(op) && !op.in_autocommit_block)
                .map(|op| {
                    Diagnostic::new(
                        self.id(),
                        self.name(),
                        self.severity(),
                        "Alembic concurrent operation is outside an autocommit block",
                        ctx.path.to_path_buf(),
                        op.span,
                    )
                    .with_help(
                        "Wrap the concurrent operation in `with op.get_context().autocommit_block():`.",
                    )
                })
                .collect();
        }
        let mut diagnostics = Vec::new();

        // Two paths to "this migration needs `atomic = False`":
        //   1. A Django op flagged as concurrent
        //      (`AddIndexConcurrently`, `RemoveIndexConcurrently`).
        //   2. A `RunSQL(...)` whose SQL contains a `CONCURRENTLY`
        //      keyword on a CREATE/DROP INDEX statement. Postgres
        //      rejects `CREATE INDEX CONCURRENTLY` inside a
        //      transaction block at runtime ("CREATE INDEX
        //      CONCURRENTLY cannot run inside a transaction
        //      block"), so an atomic migration that wraps such a
        //      RunSQL silently fails on every deploy — the rule
        //      should catch it at lint time.
        let has_concurrent = migration
            .database_effective_operations()
            .any(operation_requires_non_atomic);

        if has_concurrent && !migration.is_non_atomic {
            // Anchor the diagnostic at the `class Migration(...)` line.
            // Falling back to `Span::default()` (line 1) would make
            // inline suppression with `# zdm: ignore R004` only work at
            // the top of the file, which is rarely where users write it.
            let span = migration.class_span.unwrap_or_default();
            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    "Migration uses concurrent operations but is not marked as non-atomic",
                    ctx.path.to_path_buf(),
                    span,
                )
                .with_help(
                    "Add `atomic = False` to the Migration class to allow concurrent operations",
                ),
            );
        }

        diagnostics
    }
}

fn operation_requires_non_atomic(op: &Operation) -> bool {
    if op.op_type.is_concurrent() {
        return true;
    }
    if !matches!(
        op.op_type,
        OperationType::RunSQL | OperationType::ExecuteSql
    ) {
        return false;
    }
    if let OperationData::RunSQL(data) = &op.data {
        return any_sql_statement(&data.sql, |stmt| {
            sql_statement_contains_concurrently(stmt)
                && (sql_statement_contains_create_index(stmt)
                    || sql_statement_contains_drop_index(stmt)
                    || sql_statement_contains_reindex(stmt))
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::MigrationExtractor;
    use crate::config::Config;
    use crate::parser::ParsedMigration;
    use std::path::Path;

    const CONCURRENT_NO_ATOMIC_BAD: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):

    operations = [
        AddIndexConcurrently(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    const CONCURRENT_WITH_ATOMIC_GOOD: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    const NON_CONCURRENT_NO_ATOMIC_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[],
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R004MissingAtomicFalse, source)
    }

    #[test]
    fn generated_aerich_non_transactional_migration_allows_concurrent_index() {
        let migration = Migration::from_source(
            Path::new("migrations/models/1_20260903_jobs.py"),
            "MODELS_STATE = {}\nRUN_IN_TRANSACTION = False\nasync def upgrade(db):\n    return 'CREATE INDEX CONCURRENTLY jobs_idx ON jobs (state);'\n",
        )
        .unwrap();
        assert!(R004MissingAtomicFalse
            .check(
                &migration,
                &RuleContext {
                    config: &Config::default(),
                    path: Path::new("test.py")
                }
            )
            .is_empty());
    }

    #[test]
    fn generated_aerich_multi_statement_script_is_flagged() {
        let migration = Migration::from_source(
            Path::new("migrations/models/1_20260903_jobs.py"),
            "MODELS_STATE = {}\nRUN_IN_TRANSACTION = False\nasync def upgrade(db):\n    return 'CREATE INDEX CONCURRENTLY jobs_idx ON jobs (state); SELECT 1;'\n",
        )
        .unwrap();
        assert_eq!(
            R004MissingAtomicFalse
                .check(
                    &migration,
                    &RuleContext {
                        config: &Config::default(),
                        path: Path::new("test.py"),
                    },
                )
                .len(),
            1
        );
    }

    #[test]
    fn generated_aerich_separate_scripts_are_clean() {
        let migration = Migration::from_source(
            Path::new("migrations/models/1_20260903_jobs.py"),
            "MODELS_STATE = {}\nRUN_IN_TRANSACTION = False\nasync def upgrade(db):\n    await execute_statement(db, 'CREATE INDEX CONCURRENTLY jobs_idx ON jobs (state);')\n    await execute_statement(db, 'SELECT 1;')\n",
        )
        .unwrap();
        assert!(R004MissingAtomicFalse
            .check(
                &migration,
                &RuleContext {
                    config: &Config::default(),
                    path: Path::new("test.py"),
                },
            )
            .is_empty());
    }

    #[test]
    fn generated_aerich_multi_statement_helper_script_is_flagged() {
        let migration = Migration::from_source(
            Path::new("migrations/models/1_20260903_jobs.py"),
            "MODELS_STATE = {}\nRUN_IN_TRANSACTION = False\nasync def upgrade(db):\n    await execute_statement(db, 'DROP INDEX CONCURRENTLY jobs_idx; SELECT 1;')\n",
        )
        .unwrap();
        assert_eq!(
            R004MissingAtomicFalse
                .check(
                    &migration,
                    &RuleContext {
                        config: &Config::default(),
                        path: Path::new("test.py"),
                    },
                )
                .len(),
            1
        );
    }

    #[test]
    fn custom_aerich_migration_with_concurrent_index_is_flagged() {
        let migration = Migration::from_source(
            Path::new("migrations/models/1_20260903_jobs.py"),
            "async def upgrade(db):\n    return 'CREATE INDEX CONCURRENTLY jobs_idx ON jobs (state);'\n",
        )
        .unwrap();
        let diagnostics = R004MissingAtomicFalse.check(
            &migration,
            &RuleContext {
                config: &Config::default(),
                path: Path::new("test.py"),
            },
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0]
            .help
            .as_deref()
            .is_some_and(|help| help.contains("RUN_IN_TRANSACTION")));
    }

    #[test]
    fn later_generated_transactional_rebinding_is_flagged() {
        let migration = Migration::from_source(
            Path::new("migrations/models/1_20260903_jobs.py"),
            "MODELS_STATE = {}\nRUN_IN_TRANSACTION = False\nRUN_IN_TRANSACTION = True\nasync def upgrade(db):\n    return 'CREATE INDEX CONCURRENTLY jobs_idx ON jobs (state);'\n",
        )
        .unwrap();
        assert_eq!(
            R004MissingAtomicFalse
                .check(
                    &migration,
                    &RuleContext {
                        config: &Config::default(),
                        path: Path::new("test.py"),
                    },
                )
                .len(),
            1
        );
    }

    #[test]
    fn test_addindexconcurrently_without_atomic_false_is_flagged() {
        let diagnostics = check_migration(CONCURRENT_NO_ATOMIC_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R004");
    }

    #[test]
    fn test_concurrent_with_atomic_good() {
        let diagnostics = check_migration(CONCURRENT_WITH_ATOMIC_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_non_concurrent_no_atomic_good() {
        let diagnostics = check_migration(NON_CONCURRENT_NO_ATOMIC_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_diagnostic_anchors_at_migration_class_line() {
        // Previously R004 used `Span::default()` (line 1), so a
        // `# zdm: ignore R004` placed on or above the Migration class
        // line couldn't suppress the diagnostic — only line 1 worked.
        // Anchor the span at the class definition instead, and pin
        // that anchor by *deriving* the expected line from the
        // fixture (rather than hard-coding `6`, which would force a
        // test update on any whitespace tweak above the class).
        let diagnostics = check_migration(CONCURRENT_NO_ATOMIC_BAD);
        assert_eq!(diagnostics.len(), 1);
        let class_line = CONCURRENT_NO_ATOMIC_BAD
            .lines()
            .position(|l| l.trim_start().starts_with("class Migration"))
            .map(|i| i + 1)
            .expect("fixture contains `class Migration`");
        assert_eq!(diagnostics[0].span.start_line, class_line);
    }

    const SUPPRESSED_ABOVE_CLASS: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


# zdm: ignore R004
class Migration(migrations.Migration):

    operations = [
        AddIndexConcurrently(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    const RUNSQL_CONCURRENTLY_NO_ATOMIC_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX CONCURRENTLY idx_name ON t (c);',
        ),
    ]
"#;

    #[test]
    fn test_runsql_create_index_concurrently_without_atomic_false_fires() {
        // Postgres rejects `CREATE INDEX CONCURRENTLY` inside a
        // transaction block at runtime. A migration that wraps such
        // a RunSQL without `atomic = False` is a real bug — the
        // deploy fails every time. R004's `is_concurrent` check
        // only looked at Django op types and missed RunSQL entirely.
        let diagnostics = check_migration(RUNSQL_CONCURRENTLY_NO_ATOMIC_BAD);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert_eq!(diagnostics[0].rule_id, "R004");
    }

    const RUNSQL_MULTILINE_CONCURRENTLY_NO_ATOMIC_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='CREATE\nINDEX\nCONCURRENTLY idx_name ON t (c);',
        ),
    ]
"#;

    #[test]
    fn test_runsql_create_index_concurrently_with_escaped_whitespace_fires() {
        let diagnostics = check_migration(RUNSQL_MULTILINE_CONCURRENTLY_NO_ATOMIC_BAD);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert_eq!(diagnostics[0].rule_id, "R004");
    }

    const RUNSQL_CONCURRENTLY_WITH_ATOMIC_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX CONCURRENTLY idx_name ON t (c);',
        ),
    ]
"#;

    #[test]
    fn test_runsql_create_index_concurrently_with_atomic_false_passes() {
        // Symmetry pin: the new RunSQL branch must still honour
        // `atomic = False` — otherwise R004 would fire on every
        // legitimate concurrent RunSQL pattern.
        let diagnostics = check_migration(RUNSQL_CONCURRENTLY_WITH_ATOMIC_GOOD);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
    }

    const WRAPPED_CONCURRENT_NO_ATOMIC_BAD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                AddIndexConcurrently(
                    model_name='product',
                    index=models.Index(fields=['name'], name='product_name_idx'),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_addindexconcurrently_without_atomic_false_is_flagged() {
        let diagnostics = check_migration(WRAPPED_CONCURRENT_NO_ATOMIC_BAD);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert_eq!(diagnostics[0].rule_id, "R004");
    }

    const WRAPPED_RUNSQL_CONCURRENTLY_NO_ATOMIC_BAD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RunSQL(sql='CREATE INDEX CONCURRENTLY idx_name ON t (c);'),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_runsql_concurrently_without_atomic_false_is_flagged() {
        let diagnostics = check_migration(WRAPPED_RUNSQL_CONCURRENTLY_NO_ATOMIC_BAD);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert_eq!(diagnostics[0].rule_id, "R004");
    }

    const RUNSQL_CONCURRENTLY_IN_STRING_LITERAL_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql="INSERT INTO audit_log (msg) VALUES ('used CONCURRENTLY in prior migration');",
        ),
    ]
"#;

    #[test]
    fn test_runsql_concurrently_in_string_literal_does_not_fire() {
        // The RunSQL branch must noise-strip BEFORE matching
        // CONCURRENTLY — a CONCURRENTLY mentioned inside a single-
        // quoted string literal is just data, not a concurrent
        // index statement.
        let diagnostics = check_migration(RUNSQL_CONCURRENTLY_IN_STRING_LITERAL_GOOD);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
    }

    #[test]
    fn test_inline_ignore_above_class_suppresses_r004() {
        // End-to-end check: the diagnostic span now anchors at the
        // class, so a `# zdm: ignore R004` on the line above the class
        // falls within the suppression lookup window.
        let parsed = ParsedMigration::parse(SUPPRESSED_ABOVE_CLASS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        // Apply the registry's retain step (which is where suppression
        // actually fires); we reproduce it here in miniature so the
        // assertion is unambiguous.
        let raw = R004MissingAtomicFalse.check(
            &migration,
            &RuleContext {
                config: &Config::default(),
                path: Path::new("test.py"),
            },
        );
        let surviving: Vec<_> = raw
            .into_iter()
            .filter(|d| {
                !migration.is_rule_suppressed_at(d.rule_id, d.span.start_line, d.span.end_line)
            })
            .collect();
        assert!(surviving.is_empty());
    }
}
