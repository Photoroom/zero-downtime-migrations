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

/// Check if `text` contains a `keyword=value` assignment that sits at a
/// keyword-argument boundary inside a call. Both `null=True` and
/// `null = True` match, but `not_null=True` (no boundary before) and
/// `null=Truthy` (no boundary after the value) do not.
fn contains_keyword_assignment(text: &str, keyword: &str, value: &str) -> bool {
    let normalized = strip_whitespace(text);
    let value_bytes = value.as_bytes();
    for after_eq in keyword_eq_positions(&normalized, keyword) {
        if !normalized[after_eq..].starts_with(value_bytes) {
            continue;
        }
        let next = normalized
            .get(after_eq + value_bytes.len())
            .copied()
            .unwrap_or(b')');
        if matches!(next, b',' | b')') {
            return true;
        }
    }
    false
}

/// Check if `text` contains `keyword=...` where `keyword` is a complete
/// kwarg name (preceded by `(`, `,`, or start of text).
fn contains_keyword_with_value(text: &str, keyword: &str) -> bool {
    let normalized = strip_whitespace(text);
    !keyword_eq_positions(&normalized, keyword).is_empty()
}

/// Indices in `normalized` just after each `<keyword>=` whose keyword sits
/// at a kwarg boundary (preceded by `(`, `,`, or start of text).
fn keyword_eq_positions(normalized: &[u8], keyword: &str) -> Vec<usize> {
    let kw_eq: Vec<u8> = keyword.bytes().chain(std::iter::once(b'=')).collect();
    let mut hits = Vec::new();
    for i in 0..normalized.len() {
        if !normalized[i..].starts_with(&kw_eq) {
            continue;
        }
        let prev = if i == 0 { b'(' } else { normalized[i - 1] };
        if !matches!(prev, b'(' | b',') {
            continue;
        }
        hits.push(i + kw_eq.len());
    }
    hits
}

fn strip_whitespace(text: &str) -> Vec<u8> {
    text.bytes().filter(|b| !b.is_ascii_whitespace()).collect()
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
    ///
    /// Both `sql` and `reverse_sql` are extracted from either their keyword
    /// argument or their corresponding positional slot (0 for `sql`, 1 for
    /// `reverse_sql`, per Django's `RunSQL(sql, reverse_sql=None, ...)`
    /// signature). For each, a bare identifier value is resolved against
    /// module-level `NAME = "..."` assignments so that
    /// `RunSQL(sql=MY_SQL_CONST)` does not slip through as if the SQL were
    /// the literal text `MY_SQL_CONST`. Non-literal, non-resolvable values
    /// (function calls, lambdas, etc.) yield `None` so downstream rules
    /// fall back to the conservative "no SQL extracted" path rather than
    /// being misled by raw node text.
    fn extract_run_sql_operation(&self, args: Node) -> RunSQLOperation {
        let sql = self
            .get_keyword_arg_value(args, "sql")
            .or_else(|| self.get_nth_positional_value(args, 0))
            .and_then(|v| self.resolve_string_value(v))
            .unwrap_or_default();
        let reverse_sql = self
            .get_keyword_arg_value(args, "reverse_sql")
            .or_else(|| self.get_nth_positional_value(args, 1))
            .and_then(|v| self.resolve_string_value(v));

        RunSQLOperation { sql, reverse_sql }
    }

    /// Extract RunPython operation data.
    fn extract_run_python_operation(&self, args: Node) -> RunPythonOperation {
        let code = self
            .get_keyword_arg_string(args, "code")
            .or_else(|| self.get_nth_positional_arg(args, 0))
            .unwrap_or_default();
        // Treat an explicit Python `None` (`RunPython(forward, None)` or
        // `reverse_code=None`) as no reverse. Django itself flags that
        // shape as irreversible; without the filter, the helpers above
        // return the literal text `"None"` and downstream code mistakes
        // it for a real callable name.
        let reverse_code = self
            .get_keyword_arg_string(args, "reverse_code")
            .or_else(|| self.get_nth_positional_arg(args, 1))
            .filter(|s| s != "None");

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
            .map(|node| {
                let (module, names) = self.extract_import_parts(node);
                Import {
                    text: self.node_text(node).to_string(),
                    module,
                    names,
                    span: Span::from_node(&node),
                }
            })
            .collect()
    }

    /// For an `import_from_statement`, return the module path and the list
    /// of imported names. For a plain `import_statement` returns `(None,
    /// names_imported)` so callers can match on the absence of a module.
    fn extract_import_parts(&self, node: Node) -> (Option<String>, Vec<String>) {
        if node.kind() != "import_from_statement" {
            // `import X` / `import X as Y` — module is the dotted path.
            let mut names = Vec::new();
            for child in node.named_children(&mut node.walk()) {
                match child.kind() {
                    "dotted_name" | "identifier" => {
                        names.push(self.node_text(child).to_string());
                    }
                    "aliased_import" => {
                        if let Some(orig) = child.child_by_field_name("name") {
                            names.push(self.node_text(orig).to_string());
                        }
                    }
                    _ => {}
                }
            }
            return (None, names);
        }

        let module = node
            .child_by_field_name("module_name")
            .map(|m| self.node_text(m).to_string());

        let mut names = Vec::new();
        // Imported names appear as named children that are not the
        // module_name field. Tree-sitter-python tags `module_name` as a
        // field but the children iterator still yields it; filter by id.
        let module_id = node.child_by_field_name("module_name").map(|m| m.id());
        for child in node.named_children(&mut node.walk()) {
            if Some(child.id()) == module_id {
                continue;
            }
            match child.kind() {
                "dotted_name" | "identifier" => {
                    names.push(self.node_text(child).to_string());
                }
                "aliased_import" => {
                    if let Some(orig) = child.child_by_field_name("name") {
                        names.push(self.node_text(orig).to_string());
                    }
                }
                // `from X import *` — record as a wildcard sentinel so
                // matchers can decide whether to treat it as broad.
                "wildcard_import" => {
                    names.push("*".to_string());
                }
                _ => {}
            }
        }
        (module, names)
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

    /// Get the value node of a keyword argument, if present.
    fn get_keyword_arg_value(&self, args: Node<'a>, key: &str) -> Option<Node<'a>> {
        for child in args.children(&mut args.walk()) {
            if child.kind() != "keyword_argument" {
                continue;
            }
            let Some(name) = child.child_by_field_name("name") else {
                continue;
            };
            if self.node_text(name) != key {
                continue;
            }
            return child.child_by_field_name("value");
        }
        None
    }

    /// Get the Nth positional value-node (skipping keyword arguments and
    /// comments). Mirrors `get_nth_positional_arg` but yields the Node so
    /// callers can branch on its kind.
    fn get_nth_positional_value(&self, args: Node<'a>, n: usize) -> Option<Node<'a>> {
        let mut count = 0;
        for child in args.named_children(&mut args.walk()) {
            if matches!(child.kind(), "keyword_argument" | "comment") {
                continue;
            }
            if count == n {
                return Some(child);
            }
            count += 1;
        }
        None
    }

    /// Resolve a value node to a string: directly if it's a `string`
    /// literal, or by following an `identifier` to a module-level
    /// `NAME = "..."` assignment. Anything else (calls, lambdas, dict
    /// comprehensions, etc.) yields `None` so callers don't accidentally
    /// treat raw node text as the value.
    fn resolve_string_value(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "string" => Some(self.extract_string_value(node)),
            "identifier" => {
                let name = self.node_text(node).to_string();
                self.resolve_module_string_binding(&name)
            }
            _ => None,
        }
    }

    /// Find a module-level `name = "string-literal"` assignment and return
    /// its right-hand side. Only one level of indirection: chains like
    /// `A = B; B = "..."` are not followed.
    fn resolve_module_string_binding(&self, name: &str) -> Option<String> {
        let root = self.parsed.root_node();
        let source = self.parsed.source_bytes();
        for child in root.children(&mut root.walk()) {
            if child.kind() != "expression_statement" {
                continue;
            }
            let Some(assignment) = child.named_child(0) else {
                continue;
            };
            if assignment.kind() != "assignment" {
                continue;
            }
            let Some(left) = assignment.child_by_field_name("left") else {
                continue;
            };
            if left.utf8_text(source).ok() != Some(name) {
                continue;
            }
            let Some(right) = assignment.child_by_field_name("right") else {
                continue;
            };
            if right.kind() == "string" {
                return Some(self.extract_string_value(right));
            }
        }
        None
    }

    /// Get the textual source of the Nth positional argument, regardless
    /// of expression shape. Skips `keyword_argument` and `comment` named
    /// children so that e.g. `RunPython(forward, reverse_code=rev)`
    /// returns `forward` for `n == 0` and yields nothing for `n == 1`,
    /// and `RunPython(forward, # note\n backward)` returns `backward` for
    /// `n == 1`. The returned string is the raw node text (e.g.
    /// `migrations.RunPython.noop`, `helpers.fwd`, `(forward)`, or
    /// `make_forward()`); callers that need a specific shape should
    /// inspect it further.
    fn get_nth_positional_arg(&self, args: Node, n: usize) -> Option<String> {
        let mut count = 0;
        for child in args.named_children(&mut args.walk()) {
            if matches!(child.kind(), "keyword_argument" | "comment") {
                continue;
            }
            if count == n {
                return Some(self.node_text(child).to_string());
            }
            count += 1;
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

    const RUN_SQL_WITH_MODULE_BINDING: &str = r#"
from django.db import migrations

CREATE_SQL = "CREATE INDEX CONCURRENTLY idx ON tbl (col);"
DROP_SQL = "DROP INDEX idx;"


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(sql=CREATE_SQL, reverse_sql=DROP_SQL),
    ]
"#;

    #[test]
    fn test_extract_run_sql_resolves_identifier_bindings() {
        // The previous extractor took the raw identifier text ("CREATE_SQL")
        // as the SQL. Now we follow the assignment to the literal so R003
        // and R013 see the real SQL.
        let parsed = ParsedMigration::parse(RUN_SQL_WITH_MODULE_BINDING).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        if let OperationData::RunSQL(data) = &migration.operations[0].data {
            assert_eq!(data.sql, "CREATE INDEX CONCURRENTLY idx ON tbl (col);");
            assert_eq!(data.reverse_sql.as_deref(), Some("DROP INDEX idx;"));
        } else {
            panic!("Expected RunSQL data");
        }
    }

    const RUN_SQL_POSITIONAL_LITERAL: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL("CREATE INDEX CONCURRENTLY a ON tbl (c);", "DROP INDEX a;"),
    ]
"#;

    #[test]
    fn test_extract_run_sql_positional_reverse_sql() {
        let parsed = ParsedMigration::parse(RUN_SQL_POSITIONAL_LITERAL).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        if let OperationData::RunSQL(data) = &migration.operations[0].data {
            assert_eq!(data.sql, "CREATE INDEX CONCURRENTLY a ON tbl (c);");
            assert_eq!(data.reverse_sql.as_deref(), Some("DROP INDEX a;"));
        } else {
            panic!("Expected RunSQL data");
        }
    }

    const RUN_SQL_UNRESOLVABLE: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(sql=undefined_const),
    ]
"#;

    #[test]
    fn test_extract_run_sql_unresolvable_identifier_yields_empty() {
        // Unknown identifier: no module-level binding, so we report empty
        // SQL rather than fabricating "undefined_const" as the SQL text.
        let parsed = ParsedMigration::parse(RUN_SQL_UNRESOLVABLE).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        if let OperationData::RunSQL(data) = &migration.operations[0].data {
            assert!(data.sql.is_empty());
            assert!(data.reverse_sql.is_none());
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

    const RUN_PYTHON_WITH_INTER_ARG_COMMENT: &str = r#"
from django.db import migrations


def forward(apps, schema_editor):
    pass


def backward(apps, schema_editor):
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.RunPython(
            forward,
            # historical context for the reverse
            backward,
        ),
    ]
"#;

    #[test]
    fn test_extract_run_python_skips_inter_arg_comment() {
        // Tree-sitter-python emits comments as named children of
        // `argument_list`. Without an explicit skip, the comment between
        // `forward` and `backward` would be returned as `reverse_code` and
        // `backward` would be lost.
        let parsed = ParsedMigration::parse(RUN_PYTHON_WITH_INTER_ARG_COMMENT).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        if let OperationData::RunPython(data) = &migration.operations[0].data {
            assert_eq!(data.code, "forward");
            assert_eq!(data.reverse_code.as_deref(), Some("backward"));
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
        // Whitespace variants of the positive case.
        assert!(contains_keyword_assignment("null=True", "null", "True"));
        assert!(contains_keyword_assignment("null = True", "null", "True"));
        assert!(contains_keyword_assignment("null  =  True", "null", "True"));
        assert!(contains_keyword_assignment(
            "field(null = True)",
            "null",
            "True"
        ));

        // Wrong value.
        assert!(!contains_keyword_assignment("null=False", "null", "True"));

        // Lookalike keywords. The old substring-only implementation matched
        // these because, after stripping whitespace, "nullable=True" /
        // "not_null=True" each contain the substring "null=True".
        assert!(!contains_keyword_assignment(
            "nullable=True",
            "null",
            "True"
        ));
        assert!(!contains_keyword_assignment(
            "models.CharField(not_null=True)",
            "null",
            "True"
        ));

        // Lookalike values: "True" appearing as a prefix of "Truthy".
        assert!(!contains_keyword_assignment(
            "models.CharField(null=Truthy)",
            "null",
            "True"
        ));

        // Real usage with surrounding kwargs.
        assert!(contains_keyword_assignment(
            "models.CharField(max_length=50, null=True, default='x')",
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

        // Lookalike keyword: "my_default=5" should not match the keyword
        // "default" because `default` is not at a kwarg boundary.
        assert!(!contains_keyword_with_value("my_default=5", "default"));
        assert!(!contains_keyword_with_value(
            "models.CharField(my_default=5)",
            "default"
        ));

        // Real usage.
        assert!(contains_keyword_with_value(
            "models.CharField(max_length=50, default='x')",
            "default"
        ));
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
