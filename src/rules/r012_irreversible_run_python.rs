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

        for op in migration.database_effective_operations_of_type(OperationType::RunPython) {
            let OperationData::RunPython(data) = &op.data else {
                continue;
            };
            if data.is_reversible() {
                continue;
            }
            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    format!("RunPython '{}' has no reverse function", data.code),
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(
                    "Add a reverse function: RunPython(forward, reverse) or use \
                     RunPython(forward, migrations.RunPython.noop) if no reverse is needed",
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
        crate::rules::test_support::check_rule(&R012IrreversibleRunPython, source)
    }

    #[test]
    fn test_runpython_without_reverse_code_is_flagged() {
        let diagnostics = check_migration(IRREVERSIBLE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R012");
    }

    #[test]
    fn test_reversible_run_python_good() {
        let diagnostics = check_migration(REVERSIBLE_GOOD);
        assert!(diagnostics.is_empty());
    }

    const REVERSIBLE_NOOP_GOOD: &str = r#"
from django.db import migrations


def forward(apps, schema_editor):
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(forward, migrations.RunPython.noop),
    ]
"#;

    #[test]
    fn test_reversible_via_noop_attribute_good() {
        // The help text in this rule explicitly suggests
        // `migrations.RunPython.noop`. Verify the extractor recognises
        // attribute-access reverse callables, not just bare identifiers.
        let diagnostics = check_migration(REVERSIBLE_NOOP_GOOD);
        assert!(diagnostics.is_empty());
    }

    const REVERSIBLE_DOTTED_GOOD: &str = r#"
from django.db import migrations
import myapp.migrations.helpers as helpers


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(helpers.forward, helpers.backward),
    ]
"#;

    #[test]
    fn test_reversible_via_dotted_attribute_good() {
        let diagnostics = check_migration(REVERSIBLE_DOTTED_GOOD);
        assert!(diagnostics.is_empty());
    }

    const REVERSIBLE_VIA_CALL_GOOD: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(make_forward(), make_backward()),
    ]
"#;

    #[test]
    fn test_reversible_via_call_expression_good() {
        // Positional args don't have to be bare identifiers or attributes;
        // accept any expression so factory-style callables don't trip R012.
        let diagnostics = check_migration(REVERSIBLE_VIA_CALL_GOOD);
        assert!(diagnostics.is_empty());
    }

    // Inter-arg-comment behavior (forward and reverse identifiers must not
    // be confused with a comment node) is covered at the extractor layer
    // by `test_extract_run_python_skips_inter_arg_comment`, where we can
    // assert the actual extracted value rather than just R012's emission.
    const REVERSIBLE_PARENTHESIZED_GOOD: &str = r#"
from django.db import migrations


def forward(apps, schema_editor):
    pass


def backward(apps, schema_editor):
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython((forward), (backward)),
    ]
"#;

    #[test]
    fn test_reversible_via_parenthesized_good() {
        let diagnostics = check_migration(REVERSIBLE_PARENTHESIZED_GOOD);
        assert!(diagnostics.is_empty());
    }

    const IRREVERSIBLE_POSITIONAL_NONE_BAD: &str = r#"
from django.db import migrations


def forward(apps, schema_editor):
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(forward, None),
    ]
"#;

    const IRREVERSIBLE_KEYWORD_NONE_BAD: &str = r#"
from django.db import migrations


def forward(apps, schema_editor):
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(forward, reverse_code=None),
    ]
"#;

    #[test]
    fn test_explicit_none_positional_is_irreversible() {
        // Broadening the positional helper to accept any expression
        // started returning the literal text "None" as the reverse
        // callable. Django treats `reverse_code=None` as irreversible,
        // so R012 must still fire.
        let diagnostics = check_migration(IRREVERSIBLE_POSITIONAL_NONE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R012");
    }

    #[test]
    fn test_explicit_none_keyword_is_irreversible() {
        let diagnostics = check_migration(IRREVERSIBLE_KEYWORD_NONE_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R012");
    }
}
