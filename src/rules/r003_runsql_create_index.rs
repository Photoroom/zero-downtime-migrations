//! R003: RunSQL with CREATE INDEX
//!
//! Detects RunSQL operations that contain CREATE INDEX without CONCURRENTLY.
//! This pattern bypasses Django's concurrent operations and can cause table locks.

use crate::ast::{Migration, OperationData, OperationType};
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

        for op in migration.operations_of_type(OperationType::RunSQL) {
            if let OperationData::RunSQL(data) = &op.data {
                let sql_upper = data.sql.to_uppercase();

                // Check for CREATE INDEX without CONCURRENTLY
                if (sql_upper.contains("CREATE INDEX") || sql_upper.contains("CREATE UNIQUE INDEX"))
                    && !sql_upper.contains("CONCURRENTLY")
                {
                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: "RunSQL contains CREATE INDEX without CONCURRENTLY".to_string(),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "Use CREATE INDEX CONCURRENTLY to avoid table locks".to_string(),
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
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();
        let config = Config::default();
        let ctx = RuleContext {
            config: &config,
            path: Path::new("test.py"),
        };
        R003RunSQLCreateIndex.check(&migration, &ctx)
    }

    #[test]
    fn test_runsql_create_index_bad() {
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
}
