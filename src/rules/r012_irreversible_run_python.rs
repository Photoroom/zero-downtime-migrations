//! R012: Irreversible RunPython
//!
//! Detects RunPython operations without a reverse function.
//! Without a reverse function, migrations cannot be rolled back.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects irreversible RunPython operations.
pub struct R012IrreversibleRunPython;

impl Rule for R012IrreversibleRunPython {
    fn id(&self) -> &'static str {
        "R012"
    }

    fn name(&self) -> &'static str {
        "irreversible-run-python"
    }

    fn description(&self) -> &'static str {
        "RunPython without a reverse function makes the migration irreversible. \
         Always provide a reverse function or use migrations.RunPython.noop."
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for op in migration.operations_of_type(OperationType::RunPython) {
            if let OperationData::RunPython(data) = &op.data {
                if !data.is_reversible() {
                    diagnostics.push(Diagnostic {
                        rule_id: self.id(),
                        rule_name: self.name(),
                        message: format!("RunPython '{}' has no reverse function", data.code),
                        severity: self.severity(),
                        path: ctx.path.to_path_buf(),
                        span: op.span,
                        help: Some(
                            "Add a reverse function: RunPython(forward, reverse) or use \
                             RunPython(forward, migrations.RunPython.noop) if no reverse is needed"
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


def forward(apps, schema_editor):
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(forward),
    ]
"#;

    const REVERSIBLE_GOOD: &str = r#"
from django.db import migrations


def forward(apps, schema_editor):
    pass


def backward(apps, schema_editor):
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(forward, backward),
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
        R012IrreversibleRunPython.check(&migration, &ctx)
    }

    #[test]
    fn test_irreversible_run_python_warns() {
        let diagnostics = check_migration(IRREVERSIBLE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R012");
    }

    #[test]
    fn test_reversible_run_python_good() {
        let diagnostics = check_migration(REVERSIBLE_GOOD);
        assert!(diagnostics.is_empty());
    }
}
