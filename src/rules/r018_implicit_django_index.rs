//! R018: Django field and together operations that create indexes implicitly.

use crate::ast::{Migration, OperationData, OperationType};
use crate::diagnostics::{Diagnostic, Severity};
use crate::discovery::MigrationFramework;
use crate::rules::{walk_with_created_models, Rule, RuleContext};

pub struct R018ImplicitDjangoIndex;

impl Rule for R018ImplicitDjangoIndex {
    fn id(&self) -> &'static str {
        "R018"
    }
    fn name(&self) -> &'static str {
        "implicit-django-index"
    }
    fn description(&self) -> &'static str {
        "Django field uniqueness and indexes are built non-concurrently on existing tables."
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        if migration.framework != MigrationFramework::Django {
            return vec![];
        }
        let mut diagnostics = Vec::new();
        walk_with_created_models(migration, |op, created| {
            if created.contains_operation(migration, op) {
                return;
            }
            let creates_index = match &op.data {
                OperationData::Field(field)
                    if matches!(
                        op.op_type,
                        OperationType::AddField | OperationType::AlterField
                    ) =>
                {
                    field
                        .field
                        .as_ref()
                        .is_some_and(|field| field.db_index || field.is_unique)
                }
                OperationData::AlterUniqueTogether(data)
                    if op.op_type == OperationType::AlterUniqueTogether =>
                {
                    data.adds_unique_together
                }
                OperationData::AlterIndexTogether(data)
                    if op.op_type == OperationType::AlterIndexTogether =>
                {
                    data.adds_index_together
                }
                _ => false,
            };
            if !creates_index {
                return;
            }
            let severity = if op.op_type == OperationType::AlterField {
                Severity::Warning
            } else {
                self.severity()
            };
            diagnostics.push(Diagnostic::new(
                self.id(), self.name(), severity,
                "Django operation creates an index non-concurrently on an existing table",
                ctx.path.to_path_buf(), op.span,
            ).with_help(
                "Build the index concurrently in a separate migration and use SeparateDatabaseAndState to keep Django state aligned.",
            ));
        });
        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R018ImplicitDjangoIndex, source)
    }

    #[test]
    fn flags_implicit_indexes_and_together_operations_on_existing_models() {
        let diagnostics = check(
            r#"
from django.db import migrations, models
class Migration(migrations.Migration):
    operations = [
        migrations.AddField('product', 'sku', models.CharField(max_length=50, db_index=True)),
        migrations.AlterField('product', 'code', models.CharField(max_length=50, unique=True)),
        migrations.AlterUniqueTogether('product', {('sku', 'code')}),
        migrations.AlterIndexTogether('product', {('sku', 'code')}),
    ]
"#,
        );
        assert_eq!(diagnostics.len(), 4);
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert_eq!(diagnostics[1].severity, Severity::Warning);
        assert_eq!(diagnostics[2].severity, Severity::Error);
        assert_eq!(diagnostics[3].severity, Severity::Error);
    }

    #[test]
    fn warns_when_alterfield_may_restate_an_existing_index() {
        let diagnostics = check(
            r#"
from django.db import migrations, models
class Migration(migrations.Migration):
    operations = [
        migrations.AlterField('product', 'sku', models.CharField(max_length=50, unique=True)),
    ]
"#,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Severity::Warning);
    }

    #[test]
    fn ignores_empty_together_operations_and_fresh_models() {
        let diagnostics = check(
            r#"
from django.db import migrations, models
class Migration(migrations.Migration):
    operations = [
        migrations.CreateModel('Product', fields=[]),
        migrations.AddField('product', 'sku', models.CharField(max_length=50, db_index=True)),
        migrations.AlterUniqueTogether('product', set()),
        migrations.AlterUniqueTogether('product', None),
        migrations.AlterIndexTogether('product', set()),
        migrations.AlterIndexTogether('product', None),
        migrations.AlterIndexTogether('product', {('sku', 'code')}),
    ]
"#,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn ignores_ordinary_fields_without_explicit_indexes() {
        let diagnostics = check(
            r#"
from django.db import migrations, models
class Migration(migrations.Migration):
    operations = [
        migrations.AddField('product', 'sku', models.CharField(max_length=50)),
        migrations.AlterField('product', 'name', models.CharField(max_length=50)),
    ]
"#,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn flags_slug_fields_because_django_indexes_them_implicitly() {
        let diagnostics = check(
            r#"
from django.db import migrations, models
class Migration(migrations.Migration):
    operations = [
        migrations.AddField('product', 'slug', models.SlugField()),
    ]
"#,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Severity::Error);
    }

    #[test]
    fn ignores_explicitly_unindexed_slug_fields() {
        let diagnostics = check(
            r#"
from django.db import migrations, models
class Migration(migrations.Migration):
    operations = [
        migrations.AddField('product', 'slug', models.SlugField(db_index=False)),
    ]
"#,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn ignores_indexes_after_renaming_a_fresh_model() {
        let diagnostics = check(
            r#"
from django.db import migrations, models
class Migration(migrations.Migration):
    operations = [
        migrations.CreateModel('Product', fields=[]),
        migrations.RenameModel('Product', 'CatalogProduct'),
        migrations.AddField('catalogproduct', 'sku', models.CharField(max_length=50, unique=True)),
    ]
"#,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn ignores_aerich_operations() {
        let migration = Migration::from_source(
            std::path::Path::new("migrations/models/1_20260823_jobs.py"),
            r#"
async def upgrade(db):
    return "ALTER TABLE jobs ADD COLUMN sku TEXT UNIQUE;"
"#,
        )
        .unwrap();
        let config = crate::config::Config::default();
        let diagnostics = R018ImplicitDjangoIndex.check(
            &migration,
            &RuleContext {
                config: &config,
                path: migration.path.as_path(),
            },
        );
        assert!(diagnostics.is_empty());
    }
}
