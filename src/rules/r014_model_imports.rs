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
                });
            }
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::MigrationExtractor;
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

    const RELATIVE_HELPER_IMPORT_GOOD: &str = r#"
from django.db import migrations
from .models import compute_something


class Migration(migrations.Migration):
    operations = []
"#;

    #[test]
    fn test_relative_helper_import_is_not_flagged() {
        // `from .models import compute_something` imports a snake_case
        // helper from a sibling models module — not a model class. The
        // earlier substring-based check wrongly flagged this.
        let diagnostics = check_migration(RELATIVE_HELPER_IMPORT_GOOD);
        assert!(diagnostics.is_empty());
    }

    const RELATIVE_MODEL_IMPORT_BAD: &str = r#"
from django.db import migrations
from .models import Product


class Migration(migrations.Migration):
    operations = []
"#;

    #[test]
    fn test_relative_model_import_is_flagged() {
        let diagnostics = check_migration(RELATIVE_MODEL_IMPORT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R014");
    }

    const DJANGO_AUTH_USER_IMPORT_GOOD: &str = r#"
from django.db import migrations
from django.contrib.auth.models import User


class Migration(migrations.Migration):
    operations = []
"#;

    #[test]
    fn test_django_contrib_models_import_not_flagged() {
        // Imports from django.* are framework-provided model utilities,
        // not user model state. R014 is about the user's own model
        // classes whose schema may drift; framework models don't have
        // that problem.
        let diagnostics = check_migration(DJANGO_AUTH_USER_IMPORT_GOOD);
        assert!(diagnostics.is_empty());
    }

    const MIXED_NAMES_BAD: &str = r#"
from django.db import migrations
from .models import compute_something, Product


class Migration(migrations.Migration):
    operations = []
"#;

    #[test]
    fn test_mixed_names_in_one_import_is_flagged() {
        // A single import with at least one PascalCase name is enough.
        let diagnostics = check_migration(MIXED_NAMES_BAD);
        assert_eq!(diagnostics.len(), 1);
    }

    const WILDCARD_IMPORT_BAD: &str = r#"
from django.db import migrations
from .models import *


class Migration(migrations.Migration):
    operations = []
"#;

    #[test]
    fn test_wildcard_import_is_flagged() {
        // `from .models import *` brings every name in scope, including
        // model classes. We can't see the names, so the conservative call
        // is to flag.
        let diagnostics = check_migration(WILDCARD_IMPORT_BAD);
        assert_eq!(diagnostics.len(), 1);
    }

    const PLAIN_IMPORT_OF_MODELS_GOOD: &str = r#"
from django.db import migrations
import myapp.models


class Migration(migrations.Migration):
    operations = []
"#;

    #[test]
    fn test_plain_import_of_models_module_not_flagged() {
        // `import myapp.models` does not by itself bring a model class
        // into scope — subsequent use of `myapp.models.X` would, but
        // R014 only looks at imports today.
        let diagnostics = check_migration(PLAIN_IMPORT_OF_MODELS_GOOD);
        assert!(diagnostics.is_empty());
    }
}
