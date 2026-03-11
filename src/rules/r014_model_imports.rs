//! R014: Model imports in migrations
//!
//! Detects direct model imports in migration files.
//! Direct model imports can cause issues because the model's state at import time
//! may differ from its historical state during the migration.

use crate::ast::Migration;
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{Rule, RuleContext};

/// Rule that detects direct model imports in migrations.
pub struct R014ModelImports;

impl Rule for R014ModelImports {
    fn id(&self) -> &'static str {
        "R014"
    }

    fn name(&self) -> &'static str {
        "model-imports"
    }

    fn description(&self) -> &'static str {
        "Direct model imports in migrations can cause issues because the model's current \
         state may differ from its historical state. Use apps.get_model() instead."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for import in &migration.imports {
            if import.is_direct_model_import() {
                diagnostics.push(Diagnostic {
                    rule_id: self.id(),
                    rule_name: self.name(),
                    message: "Direct model import found in migration".to_string(),
                    severity: self.severity(),
                    path: ctx.path.to_path_buf(),
                    span: import.span,
                    help: Some(
                        "Use apps.get_model('app_name', 'ModelName') in RunPython to get \
                         the historical model state instead of importing directly"
                            .to_string(),
                    ),
                    fix: None,
                });
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

    const MODEL_IMPORT_BAD: &str = r#"
from django.db import migrations
from myapp.models import Product


class Migration(migrations.Migration):

    operations = []
"#;

    const NO_MODEL_IMPORT_GOOD: &str = r#"
from django.db import migrations


def forward(apps, schema_editor):
    Product = apps.get_model('myapp', 'Product')


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(forward),
    ]
"#;

    const DJANGO_MODELS_IMPORT_GOOD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = []
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
        R014ModelImports.check(&migration, &ctx)
    }

    #[test]
    fn test_model_import_bad() {
        let diagnostics = check_migration(MODEL_IMPORT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R014");
    }

    #[test]
    fn test_no_model_import_good() {
        let diagnostics = check_migration(NO_MODEL_IMPORT_GOOD);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_django_models_import_good() {
        let diagnostics = check_migration(DJANGO_MODELS_IMPORT_GOOD);
        assert!(diagnostics.is_empty());
    }
}
