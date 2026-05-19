//! Extracts typed migration operations from tree-sitter nodes.

use std::collections::{BTreeMap, BTreeSet};
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

/// Map the final function identifier of a field constructor call to a
/// Django field-type label. The list is the closed set of types any rule
/// currently inspects; an unrecognised call (a user-defined field class,
/// a third-party field) becomes "Unknown" so no rule fires on it spuriously.
fn classify_field_call(value: Node<'_>, ex: &MigrationExtractor<'_>) -> &'static str {
    const KNOWN: &[&str] = &[
        "ForeignKey",
        "CharField",
        "IntegerField",
        "BooleanField",
        "TextField",
    ];
    let Some(function) = value.child_by_field_name("function") else {
        return "Unknown";
    };
    let function_text = ex.node_text(function);
    let field_name = function_text
        .split('.')
        .next_back()
        .unwrap_or(function_text);
    KNOWN
        .iter()
        .find(|t| **t == field_name)
        .copied()
        .unwrap_or("Unknown")
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

/// `true` if the `models.<Type>(...)` call rooted at `value` has
/// a `keyword=<anything>` kwarg. Same AST-walking shape as
/// [`field_kwarg_equals`] but ignores the value.
fn field_has_kwarg(ex: &MigrationExtractor<'_>, value: Node<'_>, keyword: &str) -> bool {
    field_call_kwargs(value).any(|kw| {
        kw.child_by_field_name("name")
            .is_some_and(|n| ex.node_text(n) == keyword)
    })
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
fn parse_ignore_directive(comment: &str) -> Option<Vec<String>> {
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
}

impl<'a> MigrationExtractor<'a> {
    /// Create a new extractor for the given parsed migration.
    pub fn new(parsed: &'a ParsedMigration) -> Self {
        Self { parsed }
    }

    /// Extract a complete Migration from the parsed file.
    pub fn extract(&self, path: &Path) -> Result<Migration> {
        let (operations, wrapped_database_ops) = self.extract_operations();
        let imports = self.extract_imports();
        let is_non_atomic = self.parsed.is_non_atomic();
        let line_ignores = self.extract_line_ignores();
        let class_span = self
            .parsed
            .find_migration_class()
            .map(|n| Span::from_node(&n));

        Ok(Migration {
            path: path.to_path_buf(),
            is_non_atomic,
            operations,
            imports,
            wrapped_database_ops,
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

    /// Extract the migration's top-level operations and, alongside them,
    /// any operations wrapped in
    /// `SeparateDatabaseAndState(database_operations=[...])`. State-side
    /// wrapped ops are intentionally NOT surfaced: they're metadata-only
    /// and rules that scan for schema-locking patterns should ignore
    /// them.
    fn extract_operations(&self) -> (Vec<Operation>, Vec<Operation>) {
        let mut top_level: Vec<Operation> = Vec::new();
        let mut wrapped_database: Vec<Operation> = Vec::new();

        let Some(ops_list) = self.parsed.find_operations_list() else {
            return (top_level, wrapped_database);
        };

        for child in ops_list.children(&mut ops_list.walk()) {
            if child.kind() != "call" {
                continue;
            }
            let Some(op) = self.extract_operation(child) else {
                continue;
            };
            if let OperationData::SeparateDatabaseAndState(data) = &op.data {
                for wrapped in &data.database_operations {
                    wrapped_database.push(wrapped.clone());
                }
            }
            top_level.push(op);
        }

        (top_level, wrapped_database)
    }

    /// Iterate a `list` syntax node and extract any `call` children as
    /// operations. Shared by the top-level walk and the
    /// SeparateDatabaseAndState descent so the same extraction rules
    /// (e.g. unknown operation types) apply uniformly. Non-`list` value
    /// nodes (e.g. `database_operations=None`, a comprehension, an
    /// identifier referring to a module-level list) yield an empty
    /// vector — we only descend into a literal list. A nested
    /// `SeparateDatabaseAndState` inside `database_operations` is
    /// surfaced as a single op; we deliberately do not recurse, since
    /// the doubly-nested form has no real-world use and recursion would
    /// hide it from rules that want to flag it.
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
        })
    }

    /// Extract operation-specific data from arguments.
    fn extract_operation_data(&self, op_type: OperationType, args: Node) -> OperationData {
        match op_type {
            OperationType::AddIndex
            | OperationType::AddIndexConcurrently
            | OperationType::RemoveIndex
            | OperationType::RemoveIndexConcurrently => {
                OperationData::Index(self.extract_index_operation(op_type, args))
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
    ///
    /// Add forms (`AddIndex`/`AddIndexConcurrently`) carry their index
    /// definition nested in an `index=models.Index(...)` call, so we
    /// drill into that to recover the index name and column list.
    /// Remove forms (`RemoveIndex`/`RemoveIndexConcurrently`) just
    /// reference the index by its top-level `name=` kwarg and carry no
    /// column info.
    fn extract_index_operation(&self, op_type: OperationType, args: Node<'a>) -> IndexOperation {
        let model_name = self.get_keyword_arg_string(args, "model_name");

        let (index_name, columns) = match op_type {
            OperationType::AddIndex | OperationType::AddIndexConcurrently => {
                self.extract_inner_index_call(args)
            }
            OperationType::RemoveIndex | OperationType::RemoveIndexConcurrently => {
                (self.get_keyword_arg_string(args, "name"), Vec::new())
            }
            _ => (None, Vec::new()),
        };

        IndexOperation {
            model_name: model_name.unwrap_or_default(),
            index_name,
            columns,
        }
    }

    /// Drill into the `index=models.Index(...)` argument of an Add
    /// form and pull out the inner call's `name=` and `fields=[...]`.
    /// Returns `(None, Vec::new())` if the argument is missing, isn't a
    /// call node, or has no recognisable inner kwargs. The kwarg form
    /// is preferred but the positional form (`AddIndex('model', index)`
    /// — Django's signature is `(model_name, index)`) is also accepted.
    /// Non-literal `fields` values (an identifier, a comprehension)
    /// yield an empty column vec — we deliberately don't try to
    /// resolve them.
    fn extract_inner_index_call(&self, args: Node<'a>) -> (Option<String>, Vec<String>) {
        let index_value = self
            .get_keyword_arg_value(args, "index")
            .or_else(|| self.get_nth_positional_value(args, 1));
        let Some(index_value) = index_value else {
            return (None, Vec::new());
        };
        if index_value.kind() != "call" {
            return (None, Vec::new());
        }
        let Some(inner_args) = index_value.child_by_field_name("arguments") else {
            return (None, Vec::new());
        };
        let inner_name = self.get_keyword_arg_string(inner_args, "name");
        let columns = self
            .get_keyword_arg_value(inner_args, "fields")
            .map(|node| self.extract_string_list(node))
            .unwrap_or_default();
        (inner_name, columns)
    }

    /// Extract the string elements of a Python `list` literal. Returns
    /// an empty vec for any non-`list` node and silently skips
    /// non-string children (e.g. an `F('expr')` mixed into a fields
    /// list) so we don't fabricate column names.
    fn extract_string_list(&self, node: Node<'_>) -> Vec<String> {
        let mut out = Vec::new();
        if node.kind() != "list" {
            return out;
        }
        for child in node.named_children(&mut node.walk()) {
            // Accept `string` and `concatenated_string` (Python's
            // implicit-concatenation form `"a" "b"`); the latter
            // is rare in migrations but tree-sitter wraps it in a
            // distinct node so the bare `string` filter would
            // otherwise silently drop adjacent literals.
            if matches!(child.kind(), "string" | "concatenated_string") {
                out.push(self.extract_string_value(child));
            }
        }
        out
    }

    /// Extract CreateModel operation data.
    fn extract_create_model_operation(&self, args: Node) -> ModelOperation {
        let name = self.get_keyword_arg_string(args, "name");

        ModelOperation {
            name: name.unwrap_or_default(),
            old_name: None,
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
    ///
    /// The shape is `migrations.AddField(field=models.CharField(...))`
    /// (or `ForeignKey`, `IntegerField`, etc.). We find the
    /// `field=` kwarg, then descend the `models.<Type>(...)`
    /// call to read its `null=` and `default=` kwargs from the
    /// AST directly — no raw-text scanning, no keyword-boundary
    /// gymnastics on a normalised byte buffer.
    fn extract_field_info(&self, args: Node) -> Option<FieldInfo> {
        let value = self.get_keyword_arg_value(args, "field")?;
        Some(FieldInfo {
            field_type: classify_field_call(value, self).to_string(),
            is_nullable: field_kwarg_equals(self, value, "null", "True"),
            has_default: field_has_kwarg(self, value, "default"),
        })
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
            .map(|node| self.extract_operations_from_list(node))
            .unwrap_or_default();
        let state_operations = state_operations_node
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
            _ => true,
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
                self.resolve_module_string_binding(&name)
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
            if matches!(right.kind(), "string" | "concatenated_string") {
                return Some(self.extract_string_value(right));
            }
        }
        None
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
    fn extract_string_value(&self, node: Node) -> String {
        match node.kind() {
            "string" => {
                let mut out = String::new();
                for child in node.children(&mut node.walk()) {
                    match child.kind() {
                        "string_content" => out.push_str(self.node_text(child)),
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
    fn test_extract_index_operation() {
        let parsed = ParsedMigration::parse(MULTI_OPERATION_MIGRATION).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let add_index = &migration.operations[1];
        assert_eq!(add_index.op_type, OperationType::AddIndex);

        if let OperationData::Index(data) = &add_index.data {
            assert_eq!(data.model_name, "product");
            // The fixture has `index=models.Index(fields=['name'], name='product_name_idx')`.
            assert_eq!(data.index_name.as_deref(), Some("product_name_idx"));
            assert_eq!(data.columns, vec!["name".to_string()]);
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
    fn test_custom_field_name_containing_foreign_key_is_unknown() {
        let parsed = ParsedMigration::parse(CUSTOM_FOREIGN_KEY_FIELD).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Field(data) = &migration.operations[0].data else {
            panic!("Expected Field data");
        };
        assert_eq!(
            data.field.as_ref().map(|field| field.field_type.as_str()),
            Some("Unknown"),
        );
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

        // database_operations are extracted into the parallel collection.
        let kinds: Vec<_> = migration
            .wrapped_database_ops
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

        assert!(
            migration
                .wrapped_database_ops
                .iter()
                .all(|op| op.op_type != OperationType::RemoveField),
            "RemoveField is in state_operations only and must not appear in wrapped_database_ops"
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
        assert!(migration.wrapped_database_ops.is_empty());
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
        let kinds: Vec<_> = migration
            .wrapped_database_ops
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
        assert!(migration.wrapped_database_ops.is_empty());
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
        assert!(migration.wrapped_database_ops.is_empty());
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
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
    fn test_sdas_non_literal_arms_count_as_present_but_are_not_expanded() {
        let parsed = ParsedMigration::parse(SDAS_NON_LITERAL_ARMS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        assert!(migration.wrapped_database_ops.is_empty());
        let OperationData::SeparateDatabaseAndState(data) = &migration.operations[0].data else {
            panic!("Expected SeparateDatabaseAndState data");
        };
        assert!(data.has_database_operations);
        assert!(data.has_state_operations);
        assert!(data.database_operations.is_empty());
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
        assert!(migration.wrapped_database_ops.is_empty());
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

    const ADD_INDEX_MULTI_COLUMN: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='order',
            index=models.Index(fields=['customer', 'status'], name='order_customer_status_idx'),
        ),
    ]
"#;

    #[test]
    fn test_extract_index_multi_column() {
        let parsed = ParsedMigration::parse(ADD_INDEX_MULTI_COLUMN).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(data.model_name, "order");
        assert_eq!(
            data.index_name.as_deref(),
            Some("order_customer_status_idx")
        );
        assert_eq!(
            data.columns,
            vec!["customer".to_string(), "status".to_string()]
        );
    }

    const ADD_INDEX_CONCURRENTLY: &str = r#"
from django.db import models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):
    atomic = False

    operations = [
        AddIndexConcurrently(
            model_name='product',
            index=models.Index(fields=['sku'], name='product_sku_idx'),
        ),
    ]
"#;

    #[test]
    fn test_extract_add_index_concurrently_captures_columns() {
        let parsed = ParsedMigration::parse(ADD_INDEX_CONCURRENTLY).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(data.columns, vec!["sku".to_string()]);
        assert_eq!(data.index_name.as_deref(), Some("product_sku_idx"));
    }

    const REMOVE_INDEX_BY_NAME: &str = r#"
from django.db import migrations


class Migration(migrations.Migration):

    operations = [
        migrations.RemoveIndex(model_name='product', name='product_legacy_idx'),
    ]
"#;

    #[test]
    fn test_extract_remove_index_captures_name_no_columns() {
        let parsed = ParsedMigration::parse(REMOVE_INDEX_BY_NAME).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(data.model_name, "product");
        assert_eq!(data.index_name.as_deref(), Some("product_legacy_idx"));
        // RemoveIndex carries no column info.
        assert!(data.columns.is_empty());
    }

    const ADD_INDEX_POSITIONAL: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            'product',
            models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn test_extract_index_positional_args() {
        // Django's signature is `AddIndex(model_name, index)` so the
        // positional form is valid Python and a shape developers
        // actually write.
        let parsed = ParsedMigration::parse(ADD_INDEX_POSITIONAL).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        // model_name kwarg parsing remains kwarg-only, so we don't
        // assert it here — the column extraction is what this commit
        // is about.
        assert_eq!(data.index_name.as_deref(), Some("product_name_idx"));
        assert_eq!(data.columns, vec!["name".to_string()]);
    }

    const ADD_INDEX_BARE_NAME: &str = r#"
from django.db import migrations
from django.db.models import Index


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn test_extract_index_bare_name_call() {
        // `index=Index(...)` (no `models.` prefix) still parses as a
        // `call` node, so the descent should work the same.
        let parsed = ParsedMigration::parse(ADD_INDEX_BARE_NAME).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(data.columns, vec!["name".to_string()]);
        assert_eq!(data.index_name.as_deref(), Some("product_name_idx"));
    }

    const ADD_INDEX_VALUE_NOT_A_CALL: &str = r#"
from django.db import migrations


PREBUILT = None


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=PREBUILT,
        ),
    ]
"#;

    #[test]
    fn test_extract_index_value_not_a_call_yields_empty() {
        // `index=` value is an identifier, not a call. Extraction
        // should degrade gracefully (no column info, no panic).
        let parsed = ParsedMigration::parse(ADD_INDEX_VALUE_NOT_A_CALL).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(data.index_name, None);
        assert!(data.columns.is_empty());
    }

    const ADD_INDEX_FIELDS_KWARG_MISSING: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(name='product_legacy_idx'),
        ),
    ]
"#;

    #[test]
    fn test_extract_index_missing_fields_kwarg_yields_empty_columns() {
        // No `fields=` kwarg at all — extraction should still surface
        // the index name and return empty columns.
        let parsed = ParsedMigration::parse(ADD_INDEX_FIELDS_KWARG_MISSING).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(data.index_name.as_deref(), Some("product_legacy_idx"));
        assert!(data.columns.is_empty());
    }

    const ADD_INDEX_NON_LITERAL_FIELDS: &str = r#"
from django.db import migrations, models


FIELDS = ['name']


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=FIELDS, name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn test_extract_index_non_literal_fields_yields_empty_columns() {
        // `fields=FIELDS` (an identifier) is not a literal list. We
        // deliberately don't resolve it; the extractor returns an
        // empty column vec so downstream rules know they can't make
        // column-aware decisions.
        let parsed = ParsedMigration::parse(ADD_INDEX_NON_LITERAL_FIELDS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(data.index_name.as_deref(), Some("product_name_idx"));
        assert!(data.columns.is_empty());
    }

    #[test]
    fn test_sdas_nested_does_not_recurse() {
        // Doubly-nested SDaS is exotic enough that we deliberately do
        // not recurse: the outer call surfaces the inner SDaS as one op
        // in `wrapped_database_ops`, but the inner call's own
        // `database_operations` are not hoisted further. A rule that
        // wants to flag nested SDaS can inspect the surfaced op.
        let parsed = ParsedMigration::parse(SDAS_NESTED).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        assert_eq!(migration.operations.len(), 1);
        let kinds: Vec<_> = migration
            .wrapped_database_ops
            .iter()
            .map(|op| op.op_type)
            .collect();
        assert_eq!(kinds, vec![OperationType::SeparateDatabaseAndState]);
    }

    const STRING_QUOTING_VARIANTS: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(
                fields=["name", 'category', r"raw_name", """triple"""],
                name='product_multi_idx',
            ),
        ),
    ]
"#;

    #[test]
    fn test_extract_string_handles_quoting_variants() {
        // The previous `trim_matches` implementation mis-handled
        // prefixed strings (`r"raw_name"` → returned `r` glued to
        // the value) and could collapse mixed-quote contents. Pin
        // that every supported form yields the bare content.
        let parsed = ParsedMigration::parse(STRING_QUOTING_VARIANTS).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(
            data.columns,
            vec![
                "name".to_string(),
                "category".to_string(),
                "raw_name".to_string(),
                "triple".to_string(),
            ],
        );
    }

    const FSTRING_AND_CONCATENATED_SOURCE: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(
                fields=[f"prefix_{var}_suffix", "split" "joined", f"static_no_interp"],
                name='odd_idx',
            ),
        ),
    ]
"#;

    #[test]
    fn test_extract_string_handles_fstring_and_concatenated() {
        // F-strings contain an `interpolation` child whose value is
        // unknown at lint time. Concatenating the surrounding
        // `string_content` chunks would fabricate a plausible-but-
        // wrong identifier (`prefix__suffix`), which would silently
        // mismatch every catalog lookup downstream. The new
        // extractor returns an empty string for any f-string with
        // an interpolation.
        //
        // Concatenated string literals (`"a" "b"` — Python's
        // implicit-concatenation) live under a `concatenated_string`
        // parent in tree-sitter-python; the extractor now recurses
        // into the children so callers see "ab" rather than the raw
        // source span.
        let parsed = ParsedMigration::parse(FSTRING_AND_CONCATENATED_SOURCE).unwrap();
        let extractor = MigrationExtractor::new(&parsed);
        let migration = extractor.extract(Path::new("test.py")).unwrap();

        let OperationData::Index(data) = &migration.operations[0].data else {
            panic!("Expected Index data");
        };
        assert_eq!(
            data.columns,
            vec![
                // f-string with interpolation → empty
                "".to_string(),
                // adjacent literals → joined
                "splitjoined".to_string(),
                // f-string with NO interpolation → bare content
                // (equivalent to the non-f-string spelling). Pinning
                // this guards against an over-broad "any f-string is
                // empty" rewrite of the extractor.
                "static_no_interp".to_string(),
            ],
        );
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
