//! Extracts typed migration operations from tree-sitter nodes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// Whether a field constructor creates a database relation.
fn is_relation_field(value: Node<'_>, ex: &MigrationExtractor<'_>) -> bool {
    matches!(field_type(value, ex), Some("ForeignKey" | "OneToOneField"))
}

fn is_foreign_key_field(value: Node<'_>, ex: &MigrationExtractor<'_>) -> bool {
    field_type(value, ex) == Some("ForeignKey")
}

fn field_type<'a>(value: Node<'a>, ex: &'a MigrationExtractor<'a>) -> Option<&'a str> {
    let function = value.child_by_field_name("function")?;
    let function_text = ex.node_text(function);
    Some(
        function_text
            .split('.')
            .next_back()
            .unwrap_or(function_text),
    )
}

/// `true` if the `models.<Type>(...)` call rooted at `value` has
/// a `keyword=expected` kwarg. Descends the tree-sitter AST so
/// `null=True`, `null = True`, and `null=\nTrue` all match,
/// and `null_field=True` doesn't accidentally fire.
fn field_kwarg_equals(
    ex: &MigrationExtractor<'_>,
    value: Node<'_>,
    keyword: &str,
    expected: &str,
) -> bool {
    field_call_kwargs(value).any(|kw| {
        kw.child_by_field_name("name")
            .is_some_and(|n| ex.node_text(n) == keyword)
            && kw
                .child_by_field_name("value")
                .is_some_and(|v| ex.node_text(v).trim() == expected)
    })
}

/// `true` if the `models.<Type>(...)` call rooted at `value` has a
/// `keyword=<value>` kwarg where the value is a meaningful default rather than
/// Python/Django's explicit "no default" sentinels.
fn field_has_non_null_kwarg(ex: &MigrationExtractor<'_>, value: Node<'_>, keyword: &str) -> bool {
    field_call_kwargs(value).any(|kw| {
        kw.child_by_field_name("name")
            .is_some_and(|n| ex.node_text(n) == keyword)
            && kw
                .child_by_field_name("value")
                .is_some_and(|v| !is_no_default_sentinel(ex.node_text(v).trim()))
    })
}

fn is_no_default_sentinel(value: &str) -> bool {
    value == "None"
        || value
            .split('.')
            .next_back()
            .is_some_and(|name| name == "NOT_PROVIDED")
}

/// Iterate the `keyword_argument` children of the `arguments`
/// list inside a call expression. Walks into `value`'s
/// `arguments` field if `value` is a call (the normal case:
/// `value = models.CharField(...)`); otherwise yields nothing.
fn field_call_kwargs(value: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let args = if value.kind() == "call" {
        value.child_by_field_name("arguments")
    } else {
        None
    };
    let mut children: Vec<Node<'_>> = Vec::new();
    if let Some(args) = args {
        for child in args.children(&mut args.walk()) {
            if child.kind() == "keyword_argument" {
                children.push(child);
            }
        }
    }
    children.into_iter()
}

/// Parse a `# zdm: ignore RXXX[, RYYY]` comment into its rule-ID set,
/// returning `None` if the comment does not match the suppression form.
/// Accepts both `# zdm: ignore RXXX` and the slightly lax `# zdm:ignore RXXX`.
/// Rule IDs are returned in their normalised upper-case form.
pub(crate) fn parse_ignore_directive(comment: &str) -> Option<Vec<String>> {
    let body = comment.trim_start_matches('#').trim_start();
    let body = body.strip_prefix("zdm")?.trim_start();
    let body = body.strip_prefix(':')?.trim_start();
    let body = body.strip_prefix("ignore")?;
    // Require whitespace (or end-of-string) after `ignore`, so that
    // `ignored_attribute` doesn't accidentally match.
    if !body.is_empty() && !body.starts_with(char::is_whitespace) {
        return None;
    }
    let ids: Vec<String> = body
        .split(',')
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Extracts migration operations from a parsed Python file.
pub struct MigrationExtractor<'a> {
    parsed: &'a ParsedMigration,
    module_bindings: HashMap<String, Node<'a>>,
}

impl<'a> MigrationExtractor<'a> {
    /// Create a new extractor for the given parsed migration.
    pub fn new(parsed: &'a ParsedMigration) -> Self {
        Self {
            parsed,
            module_bindings: Self::collect_module_bindings(parsed),
        }
    }

    /// Index the final module assignment before the first Migration class.
    /// Operation identifiers live inside that class, so later top-level
    /// assignments cannot affect them. Keeping dynamic right-hand sides in the
    /// map is intentional: a later `SQL = build_sql()` invalidates an earlier
    /// literal instead of falling back to stale SQL.
    fn collect_module_bindings(parsed: &'a ParsedMigration) -> HashMap<String, Node<'a>> {
        let Some(class) = parsed.find_migration_class() else {
            return HashMap::new();
        };
        let root = parsed.root_node();
        let source = parsed.source_bytes();
        let mut bindings = HashMap::new();
        for child in root.children(&mut root.walk()) {
            if child.start_byte() >= class.start_byte() {
                break;
            }
            if child.kind() != "expression_statement" {
                continue;
            }
            let Some(assignment) = child.named_child(0) else {
                continue;
            };
            if assignment.kind() != "assignment" {
                continue;
            }
            let (Some(left), Some(right)) = (
                assignment.child_by_field_name("left"),
                assignment.child_by_field_name("right"),
            ) else {
                continue;
            };
            if left.kind() == "identifier" {
                if let Ok(name) = left.utf8_text(source) {
                    bindings.insert(name.to_string(), right);
                }
            }
        }
        bindings
    }

    /// Extract a complete Migration from the parsed file.
    pub fn extract(&self, path: &Path) -> Result<Migration> {
        let operations = self.extract_operations();
        let imports = self.extract_imports();
        let is_non_atomic = self.parsed.is_non_atomic();
        let line_ignores = self.extract_line_ignores();
        let class_span = self
            .parsed
            .find_migration_class()
            .map(|n| Span::from_node(&n));

        Ok(Migration {
            path: path.to_path_buf(),
            framework: crate::discovery::MigrationFramework::Django,
            is_non_atomic,
            operations,
            imports,
            class_span,
            line_ignores,
        })
    }

    /// Walk every comment in the parse tree and collect any
    /// `# zdm: ignore RXXX[, RYYY]` directives into a line → rule-ID map.
    fn extract_line_ignores(&self) -> BTreeMap<usize, BTreeSet<String>> {
        let mut map: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        for (line, text) in self.parsed.all_comments() {
            if let Some(ids) = parse_ignore_directive(&text) {
                map.entry(line).or_default().extend(ids);
            }
        }
        map
    }

    /// Extract the migration's top-level operations. Operations wrapped in
    /// `SeparateDatabaseAndState(database_operations=[...])` are retained on
    /// the wrapper op's [`SeparateDatabaseAndStateOperation::database_operations`]
    /// and surfaced for rules via [`Migration::database_effective_operations`].
    fn extract_operations(&self) -> Vec<Operation> {
        let mut top_level: Vec<Operation> = Vec::new();

        let Some(ops_list) = self.parsed.find_operations_list() else {
            return top_level;
        };

        for child in ops_list.children(&mut ops_list.walk()) {
            if child.kind() != "call" {
                continue;
            }
            if let Some(op) = self.extract_operation(child) {
                top_level.push(op);
            }
        }

        top_level
    }

    /// Iterate a `list` syntax node and extract any `call` children as
    /// operations. Shared by the top-level walk and the
    /// SeparateDatabaseAndState descent so the same extraction rules
    /// (e.g. unknown operation types) apply uniformly. Non-`list` value
    /// nodes (e.g. `database_operations=None` or a comprehension) yield
    /// an empty vector — callers resolve supported module-level list
    /// identifiers before reaching this function.
    fn extract_operations_from_list(&self, list: Node<'_>) -> Vec<Operation> {
        let mut operations = Vec::new();
        if list.kind() != "list" {
            return operations;
        }
        for child in list.children(&mut list.walk()) {
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
            table_identity: None,
            in_autocommit_block: false,
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
            OperationType::CreateModel
            | OperationType::DeleteModel
            | OperationType::RenameModel => {
                OperationData::Model(self.extract_model_operation(op_type, args))
            }
            OperationType::AddField
            | OperationType::RemoveField
            | OperationType::AlterField
            | OperationType::RenameField => {
                OperationData::Field(self.extract_field_operation(op_type, args))
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

    fn extract_index_operation(&self, args: Node<'a>) -> IndexOperation {
        IndexOperation {
            model_name: self
                .get_keyword_or_positional_string(args, "model_name", 0)
                .unwrap_or_default(),
        }
    }

    fn extract_model_operation(&self, op_type: OperationType, args: Node<'a>) -> ModelOperation {
        if op_type == OperationType::RenameModel {
            return ModelOperation {
                old_name: self.get_keyword_or_positional_string(args, "old_name", 0),
                name: self
                    .get_keyword_or_positional_string(args, "new_name", 1)
                    .unwrap_or_default(),
            };
        }
        ModelOperation {
            name: self
                .get_keyword_or_positional_string(args, "name", 0)
                .unwrap_or_default(),
            old_name: None,
        }
    }

    /// Extract field operation data.
    fn extract_field_operation(&self, op_type: OperationType, args: Node) -> FieldOperation {
        let model_name = self.get_keyword_or_positional_string(args, "model_name", 0);
        let field_name = self.get_keyword_or_positional_string(args, "name", 1);
        let old_name = self.get_keyword_arg_string(args, "old_name").or_else(|| {
            (op_type == OperationType::RenameField)
                .then(|| self.get_nth_positional_string(args, 1))
                .flatten()
        });
        let new_name = self.get_keyword_arg_string(args, "new_name").or_else(|| {
            (op_type == OperationType::RenameField)
                .then(|| self.get_nth_positional_string(args, 2))
                .flatten()
        });

        // Extract field info from the 'field' argument
        let field = if matches!(op_type, OperationType::AddField | OperationType::AlterField) {
            self.extract_field_info(args)
        } else {
            None
        };

        FieldOperation {
            model_name: model_name.unwrap_or_default(),
            field_name: field_name.unwrap_or_default(),
            old_name,
            new_name,
            field,
        }
    }

    /// Extract field info from a field argument.
    ///
    /// The shape is `migrations.AddField(field=models.CharField(...))`
    /// (or `ForeignKey`, `IntegerField`, etc.). We find the
    /// `field=` kwarg, then descend the `models.<Type>(...)`
    /// call to read its `null=`, `default=`, and `db_default=` kwargs from the
    /// AST directly — no raw-text scanning, no keyword-boundary
    /// gymnastics on a normalised byte buffer.
    fn extract_field_info(&self, args: Node) -> Option<FieldInfo> {
        let value = self
            .get_keyword_arg_value(args, "field")
            .or_else(|| self.get_nth_positional_value(args, 2))?;
        if value.kind() != "call" {
            return None;
        }
        Some(FieldInfo {
            is_relation: is_relation_field(value, self),
            is_foreign_key: is_foreign_key_field(value, self),
            db_constraint: !field_kwarg_equals(self, value, "db_constraint", "False"),
            db_index: field_kwarg_equals(self, value, "db_index", "True"),
            db_index_disabled: field_kwarg_equals(self, value, "db_index", "False"),
            is_unique: field_kwarg_equals(self, value, "unique", "True")
                || field_kwarg_equals(self, value, "primary_key", "True"),
            is_nullable: field_kwarg_equals(self, value, "null", "True"),
            has_default: field_has_non_null_kwarg(self, value, "default"),
            has_db_default: field_has_non_null_kwarg(self, value, "db_default"),
            is_type_change: false,
        })
    }

    /// Extract constraint operation data.
    fn extract_constraint_operation(&self, args: Node) -> ConstraintOperation {
        let model_name = self.get_keyword_or_positional_string(args, "model_name", 0);
        let constraint_node = self
            .get_keyword_arg_value(args, "constraint")
            .or_else(|| self.get_nth_positional_value(args, 1));
        let constraint_type = constraint_node
            .map(|node| self.extract_constraint_type_from_value(node))
            .unwrap_or(ConstraintType::Unknown);
        ConstraintOperation {
            model_name: model_name.unwrap_or_default(),
            constraint_type,
            not_valid: false,
        }
    }

    /// Extract constraint type from arguments.
    fn extract_constraint_type_from_value(&self, value: Node) -> ConstraintType {
        let constraint_name = value.child_by_field_name("function").map(|function| {
            let text = self.node_text(function);
            text.split('.').next_back().unwrap_or(text)
        });
        match constraint_name {
            Some("UniqueConstraint") => ConstraintType::Unique,
            Some("CheckConstraint") => ConstraintType::Check,
            Some("ExclusionConstraint") => ConstraintType::Exclusion,
            _ => ConstraintType::Unknown,
        }
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
        let nth_positional_text = |n: usize| {
            self.get_nth_positional_value(args, n)
                .map(|node| self.node_text(node).to_string())
        };
        let code = self
            .get_keyword_arg_string(args, "code")
            .or_else(|| nth_positional_text(0))
            .unwrap_or_default();
        // Treat an explicit Python `None` (`RunPython(forward, None)` or
        // `reverse_code=None`) as no reverse. Django itself flags that
        // shape as irreversible; without the filter, the helpers above
        // return the literal text `"None"` and downstream code mistakes
        // it for a real callable name.
        let reverse_code = self
            .get_keyword_arg_string(args, "reverse_code")
            .or_else(|| nth_positional_text(1))
            .filter(|s| s != "None");

        RunPythonOperation { code, reverse_code }
    }

    /// Extract SeparateDatabaseAndState operation data.
    fn extract_separate_db_state_operation(&self, args: Node) -> SeparateDatabaseAndStateOperation {
        let database_operations_node = self
            .get_keyword_arg_value(args, "database_operations")
            .or_else(|| self.get_nth_positional_value(args, 0));
        let state_operations_node = self
            .get_keyword_arg_value(args, "state_operations")
            .or_else(|| self.get_nth_positional_value(args, 1));

        let database_operations = database_operations_node
            .and_then(|node| self.resolve_list_node(node))
            .map(|node| self.extract_operations_from_list(node))
            .unwrap_or_default();
        let state_operations = state_operations_node
            .and_then(|node| self.resolve_list_node(node))
            .map(|node| self.extract_operations_from_list(node))
            .unwrap_or_default();
        let has_database_operations =
            self.sdas_arm_has_meaningful_operations(database_operations_node, &database_operations);
        let has_state_operations =
            self.sdas_arm_has_meaningful_operations(state_operations_node, &state_operations);

        SeparateDatabaseAndStateOperation {
            has_state_operations,
            has_database_operations,
            database_operations,
        }
    }

    fn sdas_arm_has_meaningful_operations(
        &self,
        arm: Option<Node<'_>>,
        extracted_operations: &[Operation],
    ) -> bool {
        let Some(arm) = arm else {
            return false;
        };
        match arm.kind() {
            "none" => false,
            "list" => {
                !extracted_operations.is_empty()
                    || arm
                        .named_children(&mut arm.walk())
                        .any(|child| child.kind() != "comment")
            }
            "identifier" => self
                .resolve_module_list_binding(self.node_text(arm))
                .map(|list| {
                    !extracted_operations.is_empty()
                        || list
                            .named_children(&mut list.walk())
                            .any(|child| child.kind() != "comment")
                })
                .unwrap_or(true),
            _ => true,
        }
    }

    fn resolve_list_node(&self, node: Node<'a>) -> Option<Node<'a>> {
        match node.kind() {
            "list" => Some(node),
            "identifier" => self.resolve_module_list_binding(self.node_text(node)),
            _ => None,
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

    fn get_keyword_or_positional_string(
        &self,
        args: Node<'a>,
        key: &str,
        position: usize,
    ) -> Option<String> {
        self.get_keyword_arg_string(args, key)
            .or_else(|| self.get_nth_positional_string(args, position))
    }

    fn get_nth_positional_string(&self, args: Node<'a>, n: usize) -> Option<String> {
        self.get_nth_positional_value(args, n)
            .map(|node| self.extract_string_value(node))
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
            "string" | "concatenated_string" => Some(self.extract_string_value(node)),
            "list" | "tuple" => self.resolve_sql_sequence_value(node),
            "identifier" => {
                let name = self.node_text(node).to_string();
                self.resolve_module_sql_binding(&name)
            }
            "attribute" => self.is_run_sql_noop(node).then(String::new),
            _ => None,
        }
    }

    fn resolve_sql_sequence_value(&self, node: Node<'_>) -> Option<String> {
        let mut parts = Vec::new();
        for child in node.named_children(&mut node.walk()) {
            match child.kind() {
                "string" | "concatenated_string" => {
                    parts.push(Self::trim_sequence_statement(
                        self.extract_string_value(child),
                    ));
                }
                "list" | "tuple" => {
                    parts.push(self.resolve_parameterized_sql_statement(child)?);
                }
                "comment" => {}
                _ => return None,
            }
        }
        if parts.is_empty() {
            None
        } else {
            // Django treats a sequence as separate SQL statements. Preserve
            // that boundary so statement-level rules do not let CONCURRENTLY
            // in one element exempt a blocking CREATE INDEX in another.
            Some(parts.join(";\n"))
        }
    }

    fn resolve_parameterized_sql_statement(&self, node: Node<'_>) -> Option<String> {
        let first = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() != "comment")?;
        match first.kind() {
            "string" | "concatenated_string" => Some(Self::trim_sequence_statement(
                self.extract_string_value(first),
            )),
            _ => None,
        }
    }

    fn trim_sequence_statement(statement: String) -> String {
        statement.trim_end_matches(';').to_string()
    }

    fn is_run_sql_noop(&self, node: Node<'_>) -> bool {
        matches!(
            self.node_text(node),
            "migrations.RunSQL.noop" | "RunSQL.noop"
        )
    }

    fn resolve_module_sql_binding(&self, name: &str) -> Option<String> {
        let right = *self.module_bindings.get(name)?;
        match right.kind() {
            "string" | "concatenated_string" => Some(self.extract_string_value(right)),
            "list" | "tuple" => self.resolve_sql_sequence_value(right),
            _ => None,
        }
    }

    fn resolve_module_list_binding(&self, name: &str) -> Option<Node<'a>> {
        self.module_bindings
            .get(name)
            .copied()
            .filter(|right| right.kind() == "list")
    }

    /// Extract the actual string content from a tree-sitter `string`
    /// node, concatenating every `string_content` child.
    ///
    /// The previous implementation called `trim_matches(|c| c == '"'
    /// || c == '\'')` on the raw node text. That mis-handled:
    ///   - Mixed-quote literals like `"'foo"` (would strip the inner
    ///     `'` and return `foo` instead of `'foo`).
    ///   - String prefixes like `r"foo"` (left an `r` glued to the
    ///     opening quote in the trimmed result).
    ///   - Triple-quoted strings happen to work because
    ///     `trim_matches` strips a class of characters greedily, but
    ///     the correctness was coincidental.
    ///
    /// Tree-sitter-python decomposes a `string` node into
    /// `string_start` (opening delimiter + prefix), one or more
    /// `string_content` chunks (the literal text), and `string_end`.
    /// Concatenating just the `string_content` children gives us the
    /// content verbatim regardless of quoting or prefix. Escape
    /// sequences inside the content are preserved as written, which
    /// matches the previous behaviour for any non-pathological
    /// identifier (column/index/model names don't normally contain
    /// `\` anyway).
    ///
    /// F-strings: if any child is an `interpolation` node we return
    /// an empty string rather than concatenate the surrounding
    /// `string_content` chunks. `f"prefix_{x}_suffix"` decomposes
    /// into `"prefix_"` + interpolation + `"_suffix"`; concatenating
    /// the literal parts yields the plausible-but-wrong identifier
    /// `prefix__suffix` and an extracted column/model name like that
    /// would silently mismatch every catalog lookup. Returning empty
    /// keeps the call site's existing "not found" semantics.
    ///
    /// Concatenated strings (`"a" "b"` — Python implicit
    /// concatenation): tree-sitter wraps these in a
    /// `concatenated_string` parent containing two `string` children.
    /// Recurse into the children so callers see the joined content.
    pub(crate) fn extract_string_value(&self, node: Node) -> String {
        match node.kind() {
            "string" => {
                let mut out = String::new();
                let is_raw = self.string_has_raw_prefix(node);
                for child in node.children(&mut node.walk()) {
                    match child.kind() {
                        "string_content" if is_raw => out.push_str(self.node_text(child)),
                        "string_content" => {
                            out.push_str(&decode_python_escapes_in_text(self.node_text(child)))
                        }
                        "escape_sequence" if is_raw => out.push_str(self.node_text(child)),
                        "escape_sequence" => {
                            out.push_str(&decode_python_escape(self.node_text(child)))
                        }
                        "interpolation" => return String::new(),
                        _ => {}
                    }
                }
                out
            }
            "concatenated_string" => {
                let mut out = String::new();
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "string" {
                        out.push_str(&self.extract_string_value(child));
                    }
                }
                out
            }
            _ => {
                // Fallback for non-string nodes (callers occasionally
                // pass through identifiers in error paths). Returning
                // raw text preserves the prior best-effort behaviour
                // without panicking.
                self.node_text(node).to_string()
            }
        }
    }

    fn string_has_raw_prefix(&self, node: Node<'_>) -> bool {
        node.children(&mut node.walk()).any(|child| {
            child.kind() == "string_start"
                && self
                    .node_text(child)
                    .chars()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .any(|c| matches!(c, 'r' | 'R'))
        })
    }

    /// Get the text of a node.
    fn node_text(&self, node: Node) -> &str {
        self.parsed.node_text(node)
    }
}

fn decode_python_escape(raw: &str) -> String {
    let escape = raw.strip_prefix('\\').unwrap_or(raw);
    match escape {
        "n" => "\n".to_string(),
        "r" => "\r".to_string(),
        "t" => "\t".to_string(),
        "a" => "\u{0007}".to_string(),
        "b" => "\u{0008}".to_string(),
        "f" => "\u{000c}".to_string(),
        "v" => "\u{000b}".to_string(),
        "\\" => "\\".to_string(),
        "\"" => "\"".to_string(),
        "'" => "'".to_string(),
        _ => decode_numeric_python_escape(escape).unwrap_or_else(|| raw.to_string()),
    }
}

fn decode_python_escapes_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        let Some(next) = chars.next() else {
            out.push('\\');
            break;
        };
        let mut raw = String::from("\\");
        raw.push(next);
        match next {
            'x' => raw.extend(chars.by_ref().take(2)),
            'u' => raw.extend(chars.by_ref().take(4)),
            'U' => raw.extend(chars.by_ref().take(8)),
            '0'..='7' => {
                for _ in 0..2 {
                    if chars.peek().is_some_and(|c| matches!(c, '0'..='7')) {
                        raw.push(chars.next().expect("peeked"));
                    } else {
                        break;
                    }
                }
            }
            _ => {}
        }
        out.push_str(&decode_python_escape(&raw));
    }
    out
}

fn decode_numeric_python_escape(escape: &str) -> Option<String> {
    if let Some(hex) = escape.strip_prefix('x') {
        return decode_codepoint_escape(hex, 2);
    }
    if let Some(hex) = escape.strip_prefix('u') {
        return decode_codepoint_escape(hex, 4);
    }
    if let Some(hex) = escape.strip_prefix('U') {
        return decode_codepoint_escape(hex, 8);
    }
    if escape
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '0'..='7'))
    {
        let octal: String = escape
            .chars()
            .take(3)
            .take_while(|c| matches!(c, '0'..='7'))
            .collect();
        let value = u32::from_str_radix(&octal, 8).ok()?;
        return char::from_u32(value).map(|c| c.to_string());
    }
    None
}

fn decode_codepoint_escape(hex: &str, digits: usize) -> Option<String> {
    if hex.len() != digits {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    char::from_u32(value).map(|c| c.to_string())
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
            assert!(field.is_relation);
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

    const RUN_SQL_WITH_REASSIGNED_BINDINGS: &str = r#"
from django.db import migrations

UNSAFE_LATEST = "SELECT 1"
UNSAFE_LATEST = "CREATE INDEX idx ON tbl (col)"
SAFE_LATEST = "CREATE INDEX idx ON tbl (col)"
SAFE_LATEST = "SELECT 1"
DYNAMIC_LATEST = "CREATE INDEX idx ON tbl (col)"
DYNAMIC_LATEST = build_sql()


class Migration(migrations.Migration):
    operations = [
        migrations.RunSQL(UNSAFE_LATEST),
        migrations.RunSQL(SAFE_LATEST),
        migrations.RunSQL(DYNAMIC_LATEST),
    ]


# Assignments after the Migration class cannot affect identifiers used inside it.
SAFE_LATEST = "CREATE INDEX late_idx ON tbl (col)"
"#;

    #[test]
    fn test_extract_run_sql_uses_latest_prior_binding() {
        let migration = MigrationExtractor::new(
            &ParsedMigration::parse(RUN_SQL_WITH_REASSIGNED_BINDINGS).unwrap(),
        )
        .extract(Path::new("test.py"))
        .unwrap();
        let sql: Vec<_> = migration
            .operations
            .iter()
            .map(|op| match &op.data {
                OperationData::RunSQL(data) => data.sql.as_str(),
                _ => panic!("Expected RunSQL data"),
            })
            .collect();
        assert_eq!(sql, ["CREATE INDEX idx ON tbl (col)", "SELECT 1", ""]);
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

    const RUN_SQL_NUMERIC_ESCAPES: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    operations = [
        migrations.RunSQL("CREATE\x20INDEX\u0020idx ON t (c);"),
    ]
"#;

    #[test]
    fn test_extract_run_sql_decodes_python_numeric_escapes() {
        let parsed = ParsedMigration::parse(RUN_SQL_NUMERIC_ESCAPES).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        if let OperationData::RunSQL(data) = &migration.operations[0].data {
            assert_eq!(data.sql, "CREATE INDEX idx ON t (c);");
        } else {
            panic!("Expected RunSQL data");
        }
    }

    const RUN_SQL_RAW_ESCAPES: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):
    operations = [
        migrations.RunSQL(r"CREATE\nINDEX idx ON t (c);"),
    ]
"#;

    #[test]
    fn test_extract_run_sql_preserves_raw_string_escapes() {
        let parsed = ParsedMigration::parse(RUN_SQL_RAW_ESCAPES).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        if let OperationData::RunSQL(data) = &migration.operations[0].data {
            assert_eq!(data.sql, r"CREATE\nINDEX idx ON t (c);");
        } else {
            panic!("Expected RunSQL data");
        }
    }

    const RUN_SQL_LITERAL_SHAPES: &str = r#"
from django.db import migrations


CREATE_SQL = "CREATE " "INDEX idx_a ON tbl (a);"


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(sql=CREATE_SQL),
        migrations.RunSQL(sql=["DROP INDEX idx_a;", "DROP INDEX idx_b;"]),
        migrations.RunSQL(("CREATE INDEX idx_c ON tbl (c);", "DROP INDEX idx_c;")),
    ]
"#;

    #[test]
    fn test_extract_run_sql_concatenated_and_sequence_literals() {
        let parsed = ParsedMigration::parse(RUN_SQL_LITERAL_SHAPES).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let sql: Vec<_> = migration
            .operations
            .iter()
            .map(|op| match &op.data {
                OperationData::RunSQL(data) => data.sql.as_str(),
                _ => panic!("Expected RunSQL data"),
            })
            .collect();
        assert_eq!(
            sql,
            vec![
                "CREATE INDEX idx_a ON tbl (a);",
                "DROP INDEX idx_a;\nDROP INDEX idx_b",
                "CREATE INDEX idx_c ON tbl (c);\nDROP INDEX idx_c",
            ],
        );
    }

    const RUN_SQL_UNRESOLVABLE: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RunSQL(sql=undefined_const),
    ]
"#;

    const IGNORE_DIRECTIVE_SAMPLES: &str = r#"
from django.db import migrations

# zdm: ignore R015
class Migration(migrations.Migration):

    operations = [
        migrations.AddField(  # zdm: ignore R001, R010
            model_name='order',
            name='x',
            field=None,
        ),
        migrations.RunSQL("UPDATE t SET c=1"),  # zdm:ignore R013
    ]
"#;

    #[test]
    fn test_extract_line_ignores() {
        let parsed = ParsedMigration::parse(IGNORE_DIRECTIVE_SAMPLES).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        // The comment on its own line.
        let line_r015 = *migration
            .line_ignores
            .iter()
            .find(|(_, set)| set.contains("R015"))
            .map(|(line, _)| line)
            .expect("R015 ignore present");
        assert!(line_r015 > 1, "expected a non-zero line for R015");

        // The same-line comment carries both R001 and R010.
        let ids_for_addfield = migration
            .line_ignores
            .values()
            .find(|set| set.contains("R001") && set.contains("R010"))
            .expect("R001+R010 multi-id ignore present");
        assert_eq!(ids_for_addfield.len(), 2);

        // Lax `# zdm:ignore` (no space after the colon) still works.
        assert!(migration
            .line_ignores
            .values()
            .any(|set| set.contains("R013")));
    }

    #[test]
    fn test_parse_ignore_directive_rejects_non_directives() {
        assert!(parse_ignore_directive("# normal comment").is_none());
        assert!(parse_ignore_directive("# zdm: noqa R001").is_none());
        assert!(parse_ignore_directive("# zdm: ignored R001").is_none());
        assert!(parse_ignore_directive("# zdm: ignore").is_none());
        // Case is normalised on rule IDs.
        let ids = parse_ignore_directive("# zdm: ignore r001, r002").unwrap();
        assert_eq!(ids, vec!["R001".to_string(), "R002".to_string()]);
    }

    #[test]
    fn test_is_rule_suppressed_at_within_span_and_above() {
        let parsed = ParsedMigration::parse(IGNORE_DIRECTIVE_SAMPLES).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        // Same-line suppression: AddField's `# zdm: ignore R001, R010`
        // sits on the call's first line.
        let addfield = migration
            .operations
            .iter()
            .find(|op| op.op_type == OperationType::AddField)
            .unwrap();
        assert!(migration.is_rule_suppressed_at(
            "R001",
            addfield.span.start_line,
            addfield.span.end_line,
        ));
        assert!(migration.is_rule_suppressed_at(
            "R010",
            addfield.span.start_line,
            addfield.span.end_line,
        ));

        // Different rule on the same span is not suppressed.
        assert!(!migration.is_rule_suppressed_at(
            "R016",
            addfield.span.start_line,
            addfield.span.end_line,
        ));
    }

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

    const CUSTOM_FOREIGN_KEY_FIELD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddField(
            model_name='product',
            name='external_id',
            field=fields.CustomForeignKeyField(null=True),
        ),
    ]
"#;

    #[test]
    fn test_custom_field_name_containing_foreign_key_is_not_relation() {
        let parsed = ParsedMigration::parse(CUSTOM_FOREIGN_KEY_FIELD).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Field(data) = &migration.operations[0].data else {
            panic!("Expected Field data");
        };
        assert!(!data.field.as_ref().unwrap().is_relation);
    }

    const SEPARATE_DB_AND_STATE_MIGRATION: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RunSQL(
                    sql='ALTER TABLE app_order DROP COLUMN legacy_id;',
                    reverse_sql='ALTER TABLE app_order ADD COLUMN legacy_id integer;',
                ),
                migrations.AddIndex(
                    model_name='order',
                    index=models.Index(fields=['status'], name='order_status_idx'),
                ),
            ],
            state_operations=[
                migrations.RemoveField(model_name='order', name='legacy_id'),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_database_ops_are_extracted() {
        let parsed = ParsedMigration::parse(SEPARATE_DB_AND_STATE_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        // Top-level operations are unchanged: the SeparateDatabaseAndState
        // wrapper itself is still surfaced.
        assert_eq!(migration.operations.len(), 1);
        assert_eq!(
            migration.operations[0].op_type,
            OperationType::SeparateDatabaseAndState
        );

        // database_operations are retained on the wrapper op.
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        let kinds: Vec<_> = data
            .database_operations
            .iter()
            .map(|op| op.op_type)
            .collect();
        assert_eq!(kinds, vec![OperationType::RunSQL, OperationType::AddIndex]);
    }

    #[test]
    fn test_state_operations_are_not_surfaced() {
        // state_operations are metadata-only — schema-locking rules must
        // not see them, so we deliberately drop them on the floor.
        let parsed = ParsedMigration::parse(SEPARATE_DB_AND_STATE_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        assert!(
            data.database_operations
                .iter()
                .all(|op| op.op_type != OperationType::RemoveField),
            "RemoveField is in state_operations only and must not appear in database_operations"
        );
    }

    const SDAS_WITHOUT_DB_OPS: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            state_operations=[
                migrations.RemoveField(model_name='order', name='legacy_id'),
            ],
        ),
    ]
"#;

    #[test]
    fn test_sdas_without_database_operations_kwarg_yields_empty() {
        let parsed = ParsedMigration::parse(SDAS_WITHOUT_DB_OPS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        assert!(data.database_operations.is_empty());
    }

    const SDAS_DB_OPS_POSITIONAL: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            [
                migrations.RunSQL(sql='ALTER TABLE app_order DROP COLUMN legacy_id;'),
            ],
            [
                migrations.RemoveField(model_name='order', name='legacy_id'),
            ],
        ),
    ]
"#;

    #[test]
    fn test_sdas_positional_database_operations_are_extracted() {
        // Django's signature is
        // `SeparateDatabaseAndState(database_operations=None, state_operations=None)`
        // and the positional form is valid Python that real migrations
        // sometimes use.
        let parsed = ParsedMigration::parse(SDAS_DB_OPS_POSITIONAL).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        let kinds: Vec<_> = data
            .database_operations
            .iter()
            .map(|op| op.op_type)
            .collect();
        assert_eq!(kinds, vec![OperationType::RunSQL]);
    }

    const SDAS_DB_OPS_NONE: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=None,
            state_operations=[
                migrations.RemoveField(model_name='order', name='legacy_id'),
            ],
        ),
    ]
"#;

    #[test]
    fn test_sdas_database_operations_none_yields_empty() {
        // `database_operations=None` is valid Django; the kind-check in
        // `extract_operations_from_list` should reject it without panic.
        let parsed = ParsedMigration::parse(SDAS_DB_OPS_NONE).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        assert!(data.database_operations.is_empty());
    }

    const SDAS_DB_OPS_EMPTY_LIST: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[],
            state_operations=[],
        ),
    ]
"#;

    #[test]
    fn test_sdas_empty_database_operations_yields_empty() {
        let parsed = ParsedMigration::parse(SDAS_DB_OPS_EMPTY_LIST).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        assert!(data.database_operations.is_empty());
        assert!(!data.has_database_operations);
        assert!(!data.has_state_operations);
    }

    const SDAS_NON_LITERAL_ARMS: &str = r#"
from django.db import migrations


DB_OPS = [
    migrations.RunSQL(sql='ALTER TABLE app_order DROP COLUMN legacy_id;'),
]
STATE_OPS = [
    migrations.RemoveField(model_name='order', name='legacy_id'),
]


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=DB_OPS,
            state_operations=STATE_OPS,
        ),
    ]
"#;

    #[test]
    fn test_sdas_module_list_arms_are_extracted_and_count_as_present() {
        let parsed = ParsedMigration::parse(SDAS_NON_LITERAL_ARMS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        assert!(data.has_database_operations);
        assert!(data.has_state_operations);
        assert_eq!(data.database_operations.len(), 1);
    }

    const SDAS_NON_EMPTY_UNEXPANDED_LIST_ARMS: &str = r#"
from django.db import migrations


DB_OP = migrations.RunSQL(sql='ALTER TABLE app_order DROP COLUMN legacy_id;')
STATE_OPS = [
    migrations.RemoveField(model_name='order', name='legacy_id'),
]


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[DB_OP],
            state_operations=[*STATE_OPS],
        ),
    ]
"#;

    #[test]
    fn test_sdas_non_empty_unexpanded_list_arms_count_as_present() {
        let parsed = ParsedMigration::parse(SDAS_NON_EMPTY_UNEXPANDED_LIST_ARMS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        assert!(data.has_database_operations);
        assert!(data.has_state_operations);
        assert!(data.database_operations.is_empty());
    }

    const SDAS_NESTED: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.SeparateDatabaseAndState(
                    database_operations=[
                        migrations.RunSQL(sql='SELECT 1;'),
                    ],
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_sdas_nested_does_not_recurse() {
        // The outer wrapper's `database_operations` surfaces the inner SDaS
        // as one op; the inner call's own `database_operations` are not
        // hoisted into the outer wrapper (the recursive expansion lives in
        // `database_effective_operations`). A rule can inspect the surfaced op.
        let parsed = ParsedMigration::parse(SDAS_NESTED).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        let kinds: Vec<_> = data
            .database_operations
            .iter()
            .map(|op| op.op_type)
            .collect();
        assert_eq!(kinds, vec![OperationType::SeparateDatabaseAndState]);
    }

    #[test]
    fn test_extract_without_migration_class_returns_empty_migration() {
        // A Python file that has no `class Migration` (or has only a
        // nested one) must not error — `extract` returns a Migration
        // with empty operations. The rule engine then has nothing to
        // flag, which is the correct outcome for a non-migration
        // file accidentally caught by the discovery walk.
        let source = "# not a migration\nx = 1\n";
        let parsed = ParsedMigration::parse(source).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("not_a_migration.py")).unwrap();
        assert!(migration.operations.is_empty());
        assert!(migration.class_span.is_none());
    }
}
