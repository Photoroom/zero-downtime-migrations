//! R019: table renames and drops require a staged application rollout.

use crate::ast::{Migration, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{walk_with_created_models, Rule, RuleContext};

pub struct R019TableRenameOrDrop;

impl Rule for R019TableRenameOrDrop {
    fn id(&self) -> &'static str {
        "R019"
    }

    fn name(&self) -> &'static str {
        "table-rename-or-drop"
    }

    fn description(&self) -> &'static str {
        "Renaming or dropping an existing table breaks a running application."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        walk_with_created_models(migration, |op, created| {
            if !matches!(
                op.op_type,
                OperationType::RenameModel
                    | OperationType::AlterModelTable
                    | OperationType::DeleteModel
            ) || created.contains_operation(migration, op)
            {
                return;
            }
            let mut help = "Deploy compatible application code first; use a staged migration. Renames cannot use table-retirement evidence.".to_string();
            if op.op_type == OperationType::DeleteModel
                && migration.framework.uses_sql_table_identity()
            {
                help = match (&op.table_identity, ctx.model_evidence) {
                    (Some(table), Some(evidence)) => match evidence.table_absence(migration.framework, table) {
                        Ok(()) => return,
                        Err(reason) => reason,
                    },
                    (None, _) => "cannot establish an unambiguous database table identity".into(),
                    _ => "historical table evidence requires --diff or --diff-staged, model-sources and model-retirement-ref configured in an earlier PR".into(),
                };
            }
            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    "Renaming or dropping an existing table can break a running application",
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(help),
            );
        });
        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R019TableRenameOrDrop, source)
    }

    #[test]
    fn flags_direct_table_rename_and_drop_operations() {
        let diagnostics = check(
            r#"
from django.db import migrations
class Migration(migrations.Migration):
    operations = [
        migrations.RenameModel('Product', 'CatalogProduct'),
        migrations.AlterModelTable('catalogproduct', 'catalog_product'),
        migrations.DeleteModel('catalogproduct'),
    ]
"#,
        );
        assert_eq!(diagnostics.len(), 3);
        assert!(diagnostics[0]
            .help
            .as_deref()
            .is_some_and(|help| { help.contains("staged migration") && !help.contains("ignore") }));
    }

    #[test]
    fn ignores_state_operations_but_flags_database_operations() {
        let diagnostics = check(
            r#"
from django.db import migrations
class Migration(migrations.Migration):
    operations = [
        migrations.SeparateDatabaseAndState(
            state_operations=[migrations.DeleteModel('state_only')],
            database_operations=[migrations.DeleteModel('database_effective')],
        ),
    ]
"#,
        );
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn ignores_a_fresh_table_after_rename_and_drop() {
        let diagnostics = check(
            r#"
from django.db import migrations
class Migration(migrations.Migration):
    operations = [
        migrations.CreateModel('Product', fields=[]),
        migrations.RenameModel('Product', 'CatalogProduct'),
        migrations.AlterModelTable('catalogproduct', 'catalog_product'),
        migrations.DeleteModel('catalogproduct'),
    ]
"#,
        );
        assert!(diagnostics.is_empty());
    }
}
