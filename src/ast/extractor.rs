//! Extracts typed migration operations from tree-sitter nodes.

use std::path::Path;

use tree_sitter::Node;

use super::{
    ConstraintOperation, ConstraintType, FieldInfo, FieldOperation, Import, IndexOperation,
    Migration, ModelOperation, Operation, OperationData, OperationType, RunPythonOperation,
    RunSQLOperation, SeparateDatabaseAndStateOperation,
};
use crate::diagnostics::Span;
use crate::error::Result;
use crate::parser::ParsedMigration;

/// Check if text contains a keyword assignment pattern, handling whitespace variations.
/// e.g., matches both "null=True" and "null = True"
fn contains_keyword_assignment(text: &str, keyword: &str, value: &str) -> bool {
    // Normalize whitespace: remove all spaces and check
    let normalized = text.replace(' ', "");
    let pattern = format!("{}={}", keyword, value);
    normalized.contains(&pattern)
}

/// Check if text contains a keyword assignment with any value.
/// e.g., matches "default=", "default =", "default= ", "default = "
fn contains_keyword_with_value(text: &str, keyword: &str) -> bool {
    // Check if keyword appears followed by = (with optional whitespace)
    let normalized = text.replace(' ', "");
    normalized.contains(&format!("{}=", keyword))
}

/// Extracts migration operations from a parsed Python file.
pub struct MigrationExtractor<'a> {
    parsed: &'a ParsedMigration,
}

impl<'a> MigrationExtractor<'a> {
    /// Create a new extractor for the given parsed migration.
    pub fn new(parsed: &'a ParsedMigration) -> Self {
        Self { parsed }
    }

    /// Extract a complete Migration from the parsed file.
    pub fn extract(&self, path: &Path) -> Result<Migration> {
        let operations = self.extract_operations();
        let imports = self.extract_imports();
        let is_non_atomic = self.parsed.is_non_atomic();

        // Track created models for exemption
        let created_models: Vec<String> = operations
            .iter()
            .filter(|op| op.op_type == OperationType::CreateModel)
            .filter_map(|op| match &op.data {
                OperationData::Model(m) => Some(m.name.clone()),
                _ => None,
            })
            .collect();

        Ok(Migration {
            path: path.to_path_buf(),
            is_non_atomic,
            operations,
            imports,
            created_models,
        })
    }

    /// Extract all operations from the migration.
    fn extract_operations(&self) -> Vec<Operation> {
        let Some(ops_list) = self.parsed.find_operations_list() else {
            return vec![];
        };

        let mut operations = Vec::new();

        for child in ops_list.children(&mut ops_list.walk()) {
            if child.kind() == "call" {
                if let Some(op) = self.extract_operation(child) {
                    operations.push(op);
                }
            }
        }

        operations
    }

    /// Extract a single operation from a call node.
    fn extract_operation(&self, call_node: Node) -> Option<Operation> {
        let func = call_node.child_by_field_name("function")?;
        let func_text = self.node_text(func);

        // Get the operation name (last part after dot)
        let op_name = func_text.split('.').next_back().unwrap_or(func_text);
        let op_type = OperationType::from_name(op_name);

        let args = call_node.child_by_field_name("arguments")?;
        let data = self.extract_operation_data(op_type, args);
        let span = Span::from_node(&call_node);

        Some(Operation {
            op_type,
            span,
            data,
        })
    }

    /// Extract operation-specific data from arguments.
    fn extract_operation_data(&self, op_type: OperationType, args: Node) -> OperationData {
        match op_type {
            OperationType::AddIndex
            | OperationType::AddIndexConcurrently
            | OperationType::RemoveIndex
            | OperationType::RemoveIndexConcurrently => {
                OperationData::Index(self.extract_index_operation(args))
            }
            OperationType::CreateModel => {
                OperationData::Model(self.extract_create_model_operation(args))
            }
            OperationType::AddField
            | OperationType::RemoveField
            | OperationType::AlterField
            | OperationType::RenameField => {
                OperationData::Field(self.extract_field_operation(args))
            }
            OperationType::AddConstraint | OperationType::RemoveConstraint => {
                OperationData::Constraint(self.extract_constraint_operation(args))
            }
            OperationType::RunSQL => OperationData::RunSQL(self.extract_run_sql_operation(args)),
            OperationType::RunPython => {
                OperationData::RunPython(self.extract_run_python_operation(args))
            }
            OperationType::SeparateDatabaseAndState => OperationData::SeparateDatabaseAndState(
                self.extract_separate_db_state_operation(args),
            ),
            _ => OperationData::Empty,
        }
    }

    /// Extract index operation data.
    fn extract_index_operation(&self, args: Node) -> IndexOperation {
        let model_name = self.get_keyword_arg_string(args, "model_name");
        // Index name would be nested inside the index argument

        IndexOperation {
            model_name: model_name.unwrap_or_default(),
            index_name: None,
        }
    }

    /// Extract CreateModel operation data.
    fn extract_create_model_operation(&self, args: Node) -> ModelOperation {
        let name = self.get_keyword_arg_string(args, "name");

        ModelOperation {
            name: name.unwrap_or_default(),
            old_name: None,
            // Field extraction not implemented: no current rules need CreateModel field details.
            // The CreateModel exemption logic uses model name matching, not field inspection.
            fields: vec![],
        }
    }

    /// Extract field operation data.
    fn extract_field_operation(&self, args: Node) -> FieldOperation {
        let model_name = self.get_keyword_arg_string(args, "model_name");
        let field_name = self.get_keyword_arg_string(args, "name");
        let old_name = self.get_keyword_arg_string(args, "old_name");
        let new_name = self.get_keyword_arg_string(args, "new_name");

        // Extract field info from the 'field' argument
        let field = self.extract_field_info(args);

        FieldOperation {
            model_name: model_name.unwrap_or_default(),
            field_name: field_name.unwrap_or_default(),
            old_name,
            new_name,
            field,
        }
    }

    /// Extract field info from a field argument.
    fn extract_field_info(&self, args: Node) -> Option<FieldInfo> {
        for child in args.children(&mut args.walk()) {
            if child.kind() == "keyword_argument" {
                if let Some(name) = child.child_by_field_name("name") {
                    if self.node_text(name) == "field" {
                        if let Some(value) = child.child_by_field_name("value") {
                            let raw_text = self.node_text(value).to_string();

                            // Determine field type
                            let field_type = if raw_text.contains("ForeignKey") {
                                "ForeignKey".to_string()
                            } else if raw_text.contains("CharField") {
                                "CharField".to_string()
                            } else if raw_text.contains("IntegerField") {
                                "IntegerField".to_string()
                            } else if raw_text.contains("BooleanField") {
                                "BooleanField".to_string()
                            } else if raw_text.contains("TextField") {
                                "TextField".to_string()
                            } else {
                                "Unknown".to_string()
                            };

                            // Check nullable (handles whitespace: null=True, null = True)
                            let is_nullable =
                                contains_keyword_assignment(&raw_text, "null", "True");

                            // Check default (handles whitespace: default=, default =)
                            let has_default = contains_keyword_with_value(&raw_text, "default");

                            return Some(FieldInfo {
                                field_type,
                                is_nullable,
                                has_default,
                                // FK target extraction not implemented: R006/R007 only need to know
                                // a field is a ForeignKey, not which model it references.
                                references: None,
                                raw_text,
                            });
                        }
                    }
                }
            }
        }
        None
    }

    /// Extract constraint operation data.
    fn extract_constraint_operation(&self, args: Node) -> ConstraintOperation {
        let model_name = self.get_keyword_arg_string(args, "model_name");
        let constraint_type = self.extract_constraint_type(args);

        ConstraintOperation {
            model_name: model_name.unwrap_or_default(),
            constraint_type,
            constraint_name: None,
        }
    }

    /// Extract constraint type from arguments.
    fn extract_constraint_type(&self, args: Node) -> ConstraintType {
        for child in args.children(&mut args.walk()) {
            if child.kind() == "keyword_argument" {
                if let Some(name) = child.child_by_field_name("name") {
                    if self.node_text(name) == "constraint" {
                        if let Some(value) = child.child_by_field_name("value") {
                            let text = self.node_text(value);
                            if text.contains("UniqueConstraint") {
                                return ConstraintType::Unique;
                            } else if text.contains("CheckConstraint") {
                                return ConstraintType::Check;
                            } else if text.contains("ExclusionConstraint") {
                                return ConstraintType::Exclusion;
                            }
                        }
                    }
                }
            }
        }
        ConstraintType::Unknown
    }

    /// Extract RunSQL operation data.
    fn extract_run_sql_operation(&self, args: Node) -> RunSQLOperation {
        let sql = self
            .get_keyword_arg_string(args, "sql")
            .or_else(|| self.get_first_positional_string(args))
            .unwrap_or_default();
        let reverse_sql = self.get_keyword_arg_string(args, "reverse_sql");

        RunSQLOperation { sql, reverse_sql }
    }

    /// Extract RunPython operation data.
    fn extract_run_python_operation(&self, args: Node) -> RunPythonOperation {
        let code = self
            .get_keyword_arg_string(args, "code")
            .or_else(|| self.get_nth_positional_identifier(args, 0))
            .unwrap_or_default();
        let reverse_code = self
            .get_keyword_arg_string(args, "reverse_code")
            .or_else(|| self.get_nth_positional_identifier(args, 1));

        RunPythonOperation { code, reverse_code }
    }

    /// Extract SeparateDatabaseAndState operation data.
    fn extract_separate_db_state_operation(&self, args: Node) -> SeparateDatabaseAndStateOperation {
        let mut has_state_operations = false;
        let mut has_database_operations = false;

        for child in args.children(&mut args.walk()) {
            if child.kind() == "keyword_argument" {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_text = self.node_text(name);
                    if name_text == "state_operations" {
                        has_state_operations = true;
                    } else if name_text == "database_operations" {
                        has_database_operations = true;
                    }
                }
            }
        }

        SeparateDatabaseAndStateOperation {
            has_state_operations,
            has_database_operations,
        }
    }

    /// Extract imports from the file.
    fn extract_imports(&self) -> Vec<Import> {
        self.parsed
            .get_imports()
            .into_iter()
            .map(|node| Import {
                text: self.node_text(node).to_string(),
                span: Span::from_node(&node),
            })
            .collect()
    }

    /// Get a keyword argument value as a string.
    fn get_keyword_arg_string(&self, args: Node, key: &str) -> Option<String> {
        for child in args.children(&mut args.walk()) {
            if child.kind() == "keyword_argument" {
                if let Some(name) = child.child_by_field_name("name") {
                    if self.node_text(name) == key {
                        if let Some(value) = child.child_by_field_name("value") {
                            return Some(self.extract_string_value(value));
                        }
                    }
                }
            }
        }
        None
    }

    /// Get the first positional string argument.
    fn get_first_positional_string(&self, args: Node) -> Option<String> {
        for child in args.children(&mut args.walk()) {
            if child.kind() == "string" {
                return Some(self.extract_string_value(child));
            }
        }
        None
    }

    /// Get the Nth positional identifier argument.
    fn get_nth_positional_identifier(&self, args: Node, n: usize) -> Option<String> {
        let mut count = 0;
        for child in args.children(&mut args.walk()) {
            if child.kind() == "identifier" {
                if count == n {
                    return Some(self.node_text(child).to_string());
                }
                count += 1;
            }
        }
        None
    }

    /// Extract the actual string value (removing quotes).
    fn extract_string_value(&self, node: Node) -> String {
        let text = self.node_text(node);
        // Remove surrounding quotes
        text.trim_matches(|c| c == '"' || c == '\'').to_string()
    }

    /// Get the text of a node.
    fn node_text(&self, node: Node) -> &str {
        self.parsed.node_text(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_OPERATION_MIGRATION: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = []

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
                ('name', models.CharField(max_length=255)),
            ],
        ),
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
        migrations.AddField(
            model_name='order',
            name='product',
            field=models.ForeignKey(null=True, on_delete=models.CASCADE, to='myapp.product'),
        ),
    ]
"#;

    const RUN_SQL_MIGRATION: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(
            sql='CREATE INDEX CONCURRENTLY idx ON table (col);',
            reverse_sql='DROP INDEX idx;',
        ),
    ]
"#;

    const RUN_PYTHON_MIGRATION: &str = r#"
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

    #[test]
    fn test_extract_multi_operation_migration() {
        let parsed = ParsedMigration::parse(MULTI_OPERATION_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 3);
        assert_eq!(migration.operations[0].op_type, OperationType::CreateModel);
        assert_eq!(migration.operations[1].op_type, OperationType::AddIndex);
        assert_eq!(migration.operations[2].op_type, OperationType::AddField);
    }

    #[test]
    fn test_extract_created_models() {
        let parsed = ParsedMigration::parse(MULTI_OPERATION_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.created_models.len(), 1);
        assert_eq!(migration.created_models[0], "Product");
        assert!(migration.is_model_created("product")); // Case-insensitive
        assert!(migration.is_model_created("Product"));
        assert!(!migration.is_model_created("Order"));
    }

    #[test]
    fn test_extract_index_operation() {
        let parsed = ParsedMigration::parse(MULTI_OPERATION_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let add_index = &migration.operations[1];
        assert_eq!(add_index.op_type, OperationType::AddIndex);

        if let OperationData::Index(data) = &add_index.data {
            assert_eq!(data.model_name, "product");
        } else {
            panic!("Expected Index data");
        }
    }

    #[test]
    fn test_extract_field_operation() {
        let parsed = ParsedMigration::parse(MULTI_OPERATION_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let add_field = &migration.operations[2];
        assert_eq!(add_field.op_type, OperationType::AddField);

        if let OperationData::Field(data) = &add_field.data {
            assert_eq!(data.model_name, "order");
            assert_eq!(data.field_name, "product");
            assert!(data.field.is_some());

            let field = data.field.as_ref().unwrap();
            assert_eq!(field.field_type, "ForeignKey");
            assert!(field.is_nullable);
        } else {
            panic!("Expected Field data");
        }
    }

    #[test]
    fn test_extract_run_sql_operation() {
        let parsed = ParsedMigration::parse(RUN_SQL_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let run_sql = &migration.operations[0];
        assert_eq!(run_sql.op_type, OperationType::RunSQL);

        if let OperationData::RunSQL(data) = &run_sql.data {
            assert!(data.sql.contains("CREATE INDEX"));
            assert!(data.reverse_sql.is_some());
            assert!(data.contains_create_index());
        } else {
            panic!("Expected RunSQL data");
        }
    }

    #[test]
    fn test_extract_run_python_operation() {
        let parsed = ParsedMigration::parse(RUN_PYTHON_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let run_python = &migration.operations[0];
        assert_eq!(run_python.op_type, OperationType::RunPython);

        if let OperationData::RunPython(data) = &run_python.data {
            assert_eq!(data.code, "forward");
            assert!(data.reverse_code.is_some());
            assert!(data.is_reversible());
        } else {
            panic!("Expected RunPython data");
        }
    }

    const MULTIPLE_CREATE_MODEL: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='User',
            fields=[],
        ),
        migrations.CreateModel(
            name='Profile',
            fields=[],
        ),
        migrations.AddField(
            model_name='profile',
            name='user',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.user'),
        ),
    ]
"#;

    #[test]
    fn test_multiple_create_model_exemption() {
        let parsed = ParsedMigration::parse(MULTIPLE_CREATE_MODEL).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.created_models.len(), 2);
        assert!(migration.is_model_created("User"));
        assert!(migration.is_model_created("Profile"));
        assert!(migration.is_model_created("user")); // Case insensitive
        assert!(migration.is_model_created("PROFILE")); // Case insensitive
    }

    const ADDFIELD_EXISTING_MODEL: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='existingmodel',
            name='new_field',
            field=models.CharField(max_length=255),
        ),
    ]
"#;

    #[test]
    fn test_addfield_on_existing_model_not_exempt() {
        let parsed = ParsedMigration::parse(ADDFIELD_EXISTING_MODEL).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert!(migration.created_models.is_empty());
        assert!(!migration.is_model_created("existingmodel"));
    }

    const MIXED_OPERATIONS: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='NewModel',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
            ],
        ),
        migrations.AddField(
            model_name='newmodel',
            name='status',
            field=models.CharField(max_length=50),
        ),
        migrations.AddField(
            model_name='oldmodel',
            name='reference',
            field=models.ForeignKey(on_delete=models.CASCADE, to='app.newmodel'),
        ),
    ]
"#;

    #[test]
    fn test_exemption_applies_selectively() {
        let parsed = ParsedMigration::parse(MIXED_OPERATIONS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        // NewModel was created in this migration
        assert!(migration.is_model_created("NewModel"));
        assert!(migration.is_model_created("newmodel"));

        // OldModel was not created in this migration
        assert!(!migration.is_model_created("OldModel"));
        assert!(!migration.is_model_created("oldmodel"));
    }

    #[test]
    fn test_contains_keyword_assignment() {
        // Test the helper function directly
        assert!(contains_keyword_assignment("null=True", "null", "True"));
        assert!(contains_keyword_assignment("null = True", "null", "True"));
        assert!(contains_keyword_assignment("null  =  True", "null", "True"));
        assert!(contains_keyword_assignment(
            "field(null = True)",
            "null",
            "True"
        ));
        assert!(!contains_keyword_assignment("null=False", "null", "True"));
        assert!(!contains_keyword_assignment(
            "nullable=True",
            "null",
            "True"
        ));
    }

    #[test]
    fn test_contains_keyword_with_value() {
        assert!(contains_keyword_with_value("default='foo'", "default"));
        assert!(contains_keyword_with_value("default = 'foo'", "default"));
        assert!(contains_keyword_with_value("default=None", "default"));
        assert!(!contains_keyword_with_value("no_default_here", "default"));
    }

    const FIELD_WITH_WHITESPACE_NULLABLE: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='description',
            field=models.TextField(null = True),
        ),
    ]
"#;

    #[test]
    fn test_field_nullable_with_whitespace() {
        let parsed = ParsedMigration::parse(FIELD_WITH_WHITESPACE_NULLABLE).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let add_field = &migration.operations[0];
        if let OperationData::Field(data) = &add_field.data {
            let field = data.field.as_ref().unwrap();
            assert!(
                field.is_nullable,
                "Field with 'null = True' should be detected as nullable"
            );
        } else {
            panic!("Expected Field data");
        }
    }
}
