//! Extraction of static SQL from Aerich `upgrade()` migrations.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tree_sitter::Node;

use super::{
    sql_statement_contains_create_index, sql_statement_contains_drop_index, strip_sql_noise,
    ConstraintOperation, ConstraintType, FieldInfo, FieldOperation, IndexOperation, Migration,
    ModelOperation, Operation, OperationData, OperationType, RunSQLOperation, TableIdentity,
};
use crate::ast::extractor::{parse_ignore_directive, MigrationExtractor};
use crate::diagnostics::Span;
use crate::discovery::MigrationFramework;
use crate::error::Result;
use crate::parser::ParsedMigration;

/// Extracts PostgreSQL DDL reachable from Aerich `upgrade()`.
pub(crate) struct AerichMigrationExtractor<'a> {
    parsed: &'a ParsedMigration,
}

impl<'a> AerichMigrationExtractor<'a> {
    pub(crate) fn new(parsed: &'a ParsedMigration) -> Self {
        Self { parsed }
    }

    pub(crate) fn extract(&self, path: &Path) -> Result<Migration> {
        let mut operations = Vec::new();
        let functions = self.local_functions();
        let mut visited = BTreeSet::new();
        self.extract_reachable_function(
            "upgrade",
            &functions,
            &mut visited,
            true,
            (is_generated_initial(path), self.is_generated_migration()),
            &mut operations,
        );

        Ok(Migration {
            path: path.to_path_buf(),
            framework: MigrationFramework::Aerich,
            is_non_atomic: self.is_non_transactional_generated_migration(),
            operations,
            imports: vec![],
            class_span: None,
            line_ignores: self.extract_line_ignores(),
        })
    }

    /// Aerich's generated migrations use a module-level setting to opt out of
    /// the transaction wrapper.  Keep this deliberately narrow: custom files
    /// have no static transaction contract we can safely infer.
    fn is_non_transactional_generated_migration(&self) -> bool {
        self.is_generated_migration() && self.final_run_in_transaction_is_false()
    }

    fn is_generated_migration(&self) -> bool {
        let root = self.parsed.root_node();
        let mut has_models_state = false;
        for statement in root.named_children(&mut root.walk()) {
            let Some(assignment) = statement
                .kind()
                .eq("expression_statement")
                .then(|| statement.named_child(0))
                .flatten()
                .filter(|node| node.kind() == "assignment")
            else {
                continue;
            };
            let (Some(left), Some(_right)) = (
                assignment.child_by_field_name("left"),
                assignment.child_by_field_name("right"),
            ) else {
                continue;
            };
            if self.node_text(left) == "MODELS_STATE" {
                has_models_state = true;
            }
        }
        has_models_state
    }

    fn final_run_in_transaction_is_false(&self) -> bool {
        let root = self.parsed.root_node();
        let mut run_in_transaction = None;
        for statement in root.named_children(&mut root.walk()) {
            let Some(assignment) = statement
                .kind()
                .eq("expression_statement")
                .then(|| statement.named_child(0))
                .flatten()
                .filter(|node| node.kind() == "assignment")
            else {
                continue;
            };
            let (Some(left), Some(right)) = (
                assignment.child_by_field_name("left"),
                assignment.child_by_field_name("right"),
            ) else {
                continue;
            };
            if self.node_text(left) == "RUN_IN_TRANSACTION" {
                run_in_transaction = Some(self.node_text(right).trim() == "False");
            }
        }
        run_in_transaction == Some(true)
    }

    fn local_functions(&self) -> BTreeMap<String, Node<'a>> {
        // ponytail: only follow functions declared in this migration; add import
        // resolution if Aerich migrations begin putting SQL in other modules.
        let root = self.parsed.root_node();
        root.named_children(&mut root.walk())
            .filter(|node| node.kind() == "function_definition")
            .filter_map(|node| {
                node.child_by_field_name("name")
                    .map(|name| (self.node_text(name).to_string(), node))
            })
            .collect()
    }

    fn extract_reachable_function(
        &self,
        name: &str,
        functions: &BTreeMap<String, Node<'a>>,
        visited: &mut BTreeSet<String>,
        is_unconditional: bool,
        generated: (bool, bool),
        operations: &mut Vec<Operation>,
    ) {
        let Some(function) = functions.get(name) else {
            return;
        };
        if !visited.insert(name.to_string()) {
            return;
        }
        if let Some(body) = function.child_by_field_name("body") {
            self.extract_reachable_sql(
                body,
                functions,
                visited,
                is_unconditional,
                generated,
                operations,
            );
        }
    }

    fn extract_reachable_sql(
        &self,
        node: Node<'a>,
        functions: &BTreeMap<String, Node<'a>>,
        visited: &mut BTreeSet<String>,
        is_unconditional: bool,
        generated: (bool, bool),
        operations: &mut Vec<Operation>,
    ) {
        if matches!(
            node.kind(),
            "function_definition" | "lambda" | "class_definition"
        ) {
            return;
        }
        if node.kind() == "return_statement" {
            if let Some(value) = node
                .named_child(0)
                .filter(|value| matches!(value.kind(), "string" | "concatenated_string"))
            {
                let sql = MigrationExtractor::new(self.parsed).extract_string_value(value);
                let span = Span::from_node(&value);
                self.extract_sql(&sql, span, is_unconditional, generated, operations);
            }
            return;
        }
        if node.kind() == "call" {
            self.extract_call_sql(
                node,
                functions,
                visited,
                is_unconditional,
                generated,
                operations,
            );
        }
        let is_unconditional = is_unconditional
            && !matches!(
                node.kind(),
                "if_statement"
                    | "for_statement"
                    | "while_statement"
                    | "try_statement"
                    | "match_statement"
            );
        for child in node.named_children(&mut node.walk()) {
            self.extract_reachable_sql(
                child,
                functions,
                visited,
                is_unconditional,
                generated,
                operations,
            );
        }
    }

    fn extract_call_sql(
        &self,
        call: Node<'a>,
        functions: &BTreeMap<String, Node<'a>>,
        visited: &mut BTreeSet<String>,
        is_unconditional: bool,
        generated: (bool, bool),
        operations: &mut Vec<Operation>,
    ) {
        let Some(function) = call.child_by_field_name("function") else {
            return;
        };
        let function_name = self.node_text(function);
        let args = call.child_by_field_name("arguments");

        if function_name == "execute_statement" {
            if let Some(value) = args
                .and_then(|args| args.named_child(1))
                .filter(|value| matches!(value.kind(), "string" | "concatenated_string"))
            {
                let sql = MigrationExtractor::new(self.parsed).extract_string_value(value);
                self.extract_sql(
                    &sql,
                    Span::from_node(&value),
                    is_unconditional,
                    generated,
                    operations,
                );
            }
        }

        if functions.contains_key(function_name) {
            self.extract_reachable_function(
                function_name,
                functions,
                visited,
                is_unconditional,
                generated,
                operations,
            );
        }
        if let Some(args) = args {
            for argument in args.named_children(&mut args.walk()) {
                if argument.kind() == "identifier" {
                    self.extract_reachable_function(
                        self.node_text(argument),
                        functions,
                        visited,
                        is_unconditional,
                        generated,
                        operations,
                    );
                }
            }
        }
    }

    fn extract_sql(
        &self,
        sql: &str,
        span: Span,
        create_table_is_unconditional: bool,
        generated: (bool, bool),
        operations: &mut Vec<Operation>,
    ) {
        let cleaned = strip_sql_noise(sql);
        let statements: Vec<_> = cleaned
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
            .collect();
        let multi_statement_script = statements.len() > 1;
        for statement in statements {
            let statement = statement.trim();
            let statement = if starts_with_words(statement, &["DO"]) {
                alter_table_in_do_block(statement).unwrap_or(statement)
            } else {
                statement
            };
            if let Some(operation) = self.extract_statement(
                statement,
                span,
                create_table_is_unconditional,
                generated,
                multi_statement_script,
            ) {
                let inline_unique = match &operation.data {
                    OperationData::Field(field) => field
                        .field
                        .as_ref()
                        .is_some_and(|field| field.is_unique)
                        .then(|| field.model_name.clone()),
                    _ => None,
                };
                let table_identity = operation.table_identity.clone();
                let in_autocommit_block = operation.in_autocommit_block;
                operations.push(operation);
                if let Some(model_name) = inline_unique {
                    operations.push(Operation {
                        op_type: OperationType::AddConstraint,
                        span,
                        data: OperationData::Constraint(ConstraintOperation {
                            model_name,
                            constraint_type: ConstraintType::Unique,
                            not_valid: false,
                            requires_state_only: false,
                        }),
                        table_identity,
                        in_autocommit_block,
                    });
                }
            }
        }
    }

    fn extract_statement(
        &self,
        statement: &str,
        span: Span,
        create_table_is_unconditional: bool,
        generated: (bool, bool),
        multi_statement_script: bool,
    ) -> Option<Operation> {
        let statement = statement.trim();
        if statement.is_empty() {
            return None;
        }
        let operation = |op_type, data, table_identity| Operation {
            op_type,
            span,
            data,
            table_identity,
            in_autocommit_block: multi_statement_script,
        };

        if sql_statement_contains_create_index(statement) {
            let table = identifier_after_keyword(statement, "ON")?;
            let concurrent = create_index_is_concurrent(statement);
            return Some(operation(
                if concurrent {
                    OperationType::AddIndexConcurrently
                } else {
                    OperationType::AddIndex
                },
                OperationData::Index(IndexOperation {
                    model_name: table.name.clone(),
                }),
                Some(table),
            ));
        }
        if sql_statement_contains_drop_index(statement) {
            return Some(operation(
                if drop_index_is_concurrent(statement) {
                    OperationType::RemoveIndexConcurrently
                } else {
                    OperationType::RemoveIndex
                },
                OperationData::Index(IndexOperation {
                    model_name: String::new(),
                }),
                None,
            ));
        }
        if starts_with_words(statement, &["CREATE", "TABLE"]) {
            let table = identifier_after_keyword(statement, "TABLE")?;
            let certain = create_table_is_unconditional
                && ((generated.0 || generated.1)
                    || !contains_words_in_order(statement, &["IF", "NOT", "EXISTS"]));
            return Some(operation(
                OperationType::CreateModel,
                OperationData::Model(ModelOperation {
                    name: table.name.clone(),
                    old_name: None,
                }),
                certain.then_some(table),
            ));
        }
        if generated.0
            && (starts_with_words(statement, &["COMMENT", "ON", "TABLE"])
                || starts_with_words(statement, &["COMMENT", "ON", "COLUMN"]))
        {
            return None;
        }
        if !starts_with_words(statement, &["ALTER", "TABLE"]) {
            return Some(operation(
                OperationType::ExecuteSql,
                OperationData::RunSQL(RunSQLOperation {
                    sql: statement.to_string(),
                    reverse_sql: None,
                }),
                None,
            ));
        }
        let table = identifier_after_keyword(statement, "TABLE")?;

        if contains_words_in_order(statement, &["DROP", "COLUMN"]) {
            let column = identifier_after_keyword(statement, "COLUMN")?.name;
            return Some(operation(
                OperationType::RemoveField,
                OperationData::Field(FieldOperation {
                    model_name: table.name.clone(),
                    field_name: column,
                    old_name: None,
                    new_name: None,
                    field: None,
                }),
                Some(table),
            ));
        }
        if contains_words_in_order(statement, &["RENAME", "COLUMN"]) {
            let old_name = identifier_after_keyword(statement, "COLUMN")?.name;
            let new_name = identifier_after_keyword(statement, "TO")?.name;
            return Some(operation(
                OperationType::RenameField,
                OperationData::Field(FieldOperation {
                    model_name: table.name.clone(),
                    field_name: old_name.clone(),
                    old_name: Some(old_name),
                    new_name: Some(new_name),
                    field: None,
                }),
                Some(table),
            ));
        }
        if contains_words_in_order(statement, &["ALTER", "COLUMN", "TYPE"])
            || contains_words_in_order(statement, &["ALTER", "COLUMN", "SET", "DATA", "TYPE"])
        {
            let column = identifier_after_keyword(statement, "COLUMN")?.name;
            return Some(operation(
                OperationType::AlterField,
                OperationData::Field(FieldOperation {
                    model_name: table.name.clone(),
                    field_name: column,
                    old_name: None,
                    new_name: None,
                    field: Some(FieldInfo {
                        is_relation: false,
                        is_foreign_key: false,
                        db_constraint: true,
                        db_index: false,
                        db_index_disabled: false,
                        is_unique: false,
                        is_nullable: true,
                        has_default: false,
                        has_db_default: false,
                        is_type_change: true,
                    }),
                }),
                Some(table),
            ));
        }
        if contains_words_in_order(statement, &["ALTER", "COLUMN", "SET", "NOT", "NULL"]) {
            let column = identifier_after_keyword(statement, "COLUMN")?.name;
            return Some(operation(
                OperationType::AlterField,
                OperationData::Field(FieldOperation {
                    model_name: table.name.clone(),
                    field_name: column,
                    old_name: None,
                    new_name: None,
                    field: Some(FieldInfo {
                        is_relation: false,
                        is_foreign_key: false,
                        db_constraint: true,
                        db_index: false,
                        db_index_disabled: false,
                        is_unique: false,
                        is_nullable: false,
                        has_default: false,
                        has_db_default: false,
                        is_type_change: false,
                    }),
                }),
                Some(table),
            ));
        }
        if let Some(column) = add_column_identifier(statement) {
            return Some(operation(
                OperationType::AddField,
                OperationData::Field(FieldOperation {
                    model_name: table.name.clone(),
                    field_name: column,
                    old_name: None,
                    new_name: None,
                    field: Some(FieldInfo {
                        is_relation: contains_word(statement, "REFERENCES"),
                        is_foreign_key: false,
                        db_constraint: true,
                        db_index: false,
                        db_index_disabled: false,
                        is_unique: contains_word(statement, "UNIQUE"),
                        is_nullable: !contains_words_in_order(statement, &["NOT", "NULL"]),
                        has_default: contains_word(statement, "DEFAULT"),
                        has_db_default: false,
                        is_type_change: false,
                    }),
                }),
                Some(table),
            ));
        }
        if !contains_word(statement, "ADD") || !contains_word(statement, "CONSTRAINT") {
            return Some(operation(
                OperationType::ExecuteSql,
                OperationData::RunSQL(RunSQLOperation {
                    sql: statement.to_string(),
                    reverse_sql: None,
                }),
                None,
            ));
        }
        let constraint_type = if contains_words_in_order(statement, &["FOREIGN", "KEY"]) {
            ConstraintType::ForeignKey
        } else if contains_word(statement, "CHECK") {
            ConstraintType::Check
        } else if contains_word(statement, "EXCLUDE") {
            ConstraintType::Exclusion
        } else if contains_word(statement, "UNIQUE") {
            if contains_words_in_order(statement, &["USING", "INDEX"]) {
                return None;
            }
            ConstraintType::Unique
        } else {
            return Some(operation(
                OperationType::ExecuteSql,
                OperationData::RunSQL(RunSQLOperation {
                    sql: statement.to_string(),
                    reverse_sql: None,
                }),
                None,
            ));
        };
        Some(operation(
            OperationType::AddConstraint,
            OperationData::Constraint(ConstraintOperation {
                model_name: table.name.clone(),
                constraint_type,
                not_valid: ends_with_not_valid_clause(statement),
                requires_state_only: false,
            }),
            Some(table),
        ))
    }

    fn extract_line_ignores(&self) -> BTreeMap<usize, BTreeSet<String>> {
        let mut ignores: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        for (line, text) in self.parsed.all_comments() {
            if let Some(ids) = parse_ignore_directive(&text) {
                ignores.entry(line).or_default().extend(ids);
            }
        }
        ignores
    }

    fn node_text(&self, node: Node<'_>) -> &str {
        self.parsed.node_text(node)
    }
}

fn is_generated_initial(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("0_"))
        .is_some_and(|name| {
            name.strip_suffix(".py")
                .is_some_and(|name| name.ends_with("_init"))
        })
}

fn starts_with_words(statement: &str, words: &[&str]) -> bool {
    let mut actual = sql_words(statement);
    words
        .iter()
        .all(|expected| actual.next().as_deref() == Some(*expected))
}

fn contains_words_in_order(statement: &str, words: &[&str]) -> bool {
    let mut expected = words.iter();
    let Some(mut wanted) = expected.next() else {
        return true;
    };
    for actual in sql_words(statement) {
        if actual == *wanted {
            if let Some(next) = expected.next() {
                wanted = next;
            } else {
                return true;
            }
        }
    }
    false
}

fn contains_word(statement: &str, word: &str) -> bool {
    sql_words(statement).any(|actual| actual == word)
}

/// Return the column named by `ADD [COLUMN] [IF NOT EXISTS] <definition>`.
/// Table constraints begin with reserved words and must remain constraints.
fn add_column_identifier(statement: &str) -> Option<String> {
    let upper = statement.to_ascii_uppercase();
    let start = upper.match_indices("ADD").find_map(|(start, _)| {
        let before = upper.as_bytes().get(start.wrapping_sub(1)).copied();
        let end = start + "ADD".len();
        let after = upper.as_bytes().get(end).copied();
        (before.is_none_or(|byte| !is_identifier_byte(byte))
            && after.is_none_or(|byte| !is_identifier_byte(byte)))
        .then_some(end)
    })?;
    let mut definition = statement[start..].trim_start();
    if let Some(rest) = strip_leading_word(definition, "COLUMN") {
        definition = rest.trim_start();
    }
    let column = parse_identifier(definition)?.name;
    (!matches!(
        column.to_ascii_uppercase().as_str(),
        "CONSTRAINT" | "UNIQUE" | "PRIMARY" | "FOREIGN" | "CHECK" | "EXCLUDE"
    ))
    .then_some(column)
}

/// Aerich migrations sometimes use `DO $$ ... $$` to make adding a constraint
/// idempotent. PostgreSQL permits semicolons inside that block, so split SQL
/// leaves the first embedded `ALTER TABLE` prefixed by `DO $$ BEGIN`.
fn alter_table_in_do_block(statement: &str) -> Option<&str> {
    let upper = statement.to_ascii_uppercase();
    upper.match_indices("ALTER TABLE").find_map(|(start, _)| {
        let before = upper.as_bytes().get(start.wrapping_sub(1)).copied();
        let end = start + "ALTER TABLE".len();
        let after = upper.as_bytes().get(end).copied();
        (before.is_none_or(|byte| !is_identifier_byte(byte))
            && after.is_none_or(|byte| !is_identifier_byte(byte)))
        .then_some(&statement[start..])
    })
}

fn create_index_is_concurrent(statement: &str) -> bool {
    let mut words = sql_words(statement);
    if words.next().as_deref() != Some("CREATE") {
        return false;
    }
    let first = words.next();
    if first.as_deref() == Some("UNIQUE") && words.next().as_deref() != Some("INDEX") {
        return false;
    }
    if first.as_deref() != Some("UNIQUE") && first.as_deref() != Some("INDEX") {
        return false;
    }
    words.next().as_deref() == Some("CONCURRENTLY")
}

fn drop_index_is_concurrent(statement: &str) -> bool {
    let mut words = sql_words(statement);
    matches!(words.next().as_deref(), Some("DROP"))
        && matches!(words.next().as_deref(), Some("INDEX"))
        && matches!(words.next().as_deref(), Some("CONCURRENTLY"))
}

fn ends_with_not_valid_clause(statement: &str) -> bool {
    statement
        .trim_end()
        .to_ascii_uppercase()
        .strip_suffix("NOT VALID")
        .is_some_and(|before| before.ends_with(char::is_whitespace))
}

fn sql_words(statement: &str) -> impl Iterator<Item = String> + '_ {
    statement
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_uppercase)
}

fn identifier_after_keyword(statement: &str, keyword: &str) -> Option<TableIdentity> {
    let upper = statement.to_ascii_uppercase();
    let start = upper.match_indices(keyword).find_map(|(start, _)| {
        let before = upper.as_bytes().get(start.wrapping_sub(1)).copied();
        let end = start + keyword.len();
        let after = upper.as_bytes().get(end).copied();
        (before.is_none_or(|byte| !is_identifier_byte(byte))
            && after.is_none_or(|byte| !is_identifier_byte(byte)))
        .then_some(end)
    })?;
    parse_identifier(&statement[start..])
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn parse_identifier(value: &str) -> Option<TableIdentity> {
    let mut value = value.trim_start();
    for keyword in ["IF", "NOT", "EXISTS"] {
        if let Some(rest) = strip_leading_word(value, keyword) {
            value = rest.trim_start();
        }
    }
    let (first, rest) = parse_identifier_part(value)?;
    let rest = rest.trim_start();
    if let Some(rest) = rest.strip_prefix('.') {
        let (name, _) = parse_identifier_part(rest.trim_start())?;
        Some(TableIdentity {
            schema: Some(first),
            name,
        })
    } else {
        Some(TableIdentity {
            schema: None,
            name: first,
        })
    }
}

fn strip_leading_word<'a>(value: &'a str, word: &str) -> Option<&'a str> {
    let prefix = value.get(..word.len())?;
    (prefix.eq_ignore_ascii_case(word)
        && value
            .as_bytes()
            .get(word.len())
            .is_none_or(|byte| !is_identifier_byte(*byte)))
    .then_some(&value[word.len()..])
}

fn parse_identifier_part(value: &str) -> Option<(String, &str)> {
    if let Some(rest) = value.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some((rest[..end].to_string(), &rest[end + 1..]));
    }
    let end = value
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(value.len());
    (end > 0).then(|| (value[..end].to_string(), &value[end..]))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::ast::{Migration, OperationData, OperationType};
    use crate::config::Config;
    use crate::discovery::MigrationFramework;
    use crate::rules::RuleRegistry;

    fn migration(source: &str) -> Migration {
        Migration::from_source(Path::new("migrations/models/1_20260823_jobs.py"), source).unwrap()
    }

    #[test]
    fn extracts_literal_upgrade_sql_but_ignores_downgrade_and_dynamic_values() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "CREATE INDEX jobs_state_idx ON jobs (state);"

async def downgrade(db):
    return "DROP INDEX jobs_state_idx;"

def dynamic_sql():
    return "DROP COLUMN ignored;"
"#,
        );
        assert_eq!(migration.framework, MigrationFramework::Aerich);
        assert_eq!(migration.operations.len(), 1);
        assert_eq!(migration.operations[0].op_type, OperationType::AddIndex);
    }

    #[test]
    fn maps_postgres_safety_operations() {
        let migration = migration(
            r#"
async def upgrade(db):
    return """
        ALTER TABLE public.jobs DROP COLUMN legacy;
        ALTER TABLE public.jobs RENAME COLUMN old TO new;
        ALTER TABLE public.jobs ALTER COLUMN state SET NOT NULL;
        ALTER TABLE public.jobs ADD CONSTRAINT jobs_check CHECK (state <> '');
        ALTER TABLE public.jobs ADD CONSTRAINT jobs_fk FOREIGN KEY (owner_id) REFERENCES owner (id) NOT VALID;
    """
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.rule_id)
            .collect();
        assert_eq!(ids, vec!["R005", "R011", "R015", "R017"]);
        assert!(migration.operations.iter().any(|operation| {
            matches!(
                &operation.data,
                OperationData::Constraint(constraint) if constraint.not_valid
            )
        }));
    }

    #[test]
    fn maps_generated_add_columns_but_not_table_constraints() {
        let migration = migration(
            r#"
async def upgrade(db):
    return '''
        ALTER TABLE jobs ADD "column" TEXT DEFAULT 'new';
        ALTER TABLE jobs ADD IF NOT EXISTS owner_id UUID NOT NULL REFERENCES owners (id);
        ALTER TABLE jobs ADD UNIQUE (owner_id);
        ALTER TABLE jobs ADD CONSTRAINT jobs_owner_check CHECK (owner_id IS NOT NULL);
    '''
"#,
        );
        assert_eq!(
            migration
                .operations
                .iter()
                .map(|operation| operation.op_type)
                .collect::<Vec<_>>(),
            vec![
                OperationType::AddField,
                OperationType::AddField,
                OperationType::ExecuteSql,
                OperationType::AddConstraint,
            ]
        );
        let fields = migration
            .operations
            .iter()
            .filter_map(|operation| match &operation.data {
                OperationData::Field(field) => field.field.as_ref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(fields[0].has_default && fields[0].is_nullable);
        assert!(fields[1].is_relation && !fields[1].is_nullable);
    }

    #[test]
    fn inline_unique_column_is_checked_on_existing_tables_only() {
        let existing = migration(
            r#"
async def upgrade(db):
    return 'ALTER TABLE "jobs" ADD "ref" VARCHAR(64) UNIQUE;'
"#,
        );
        assert_eq!(
            RuleRegistry::new()
                .check(&existing, &Config::default())
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R002"]
        );

        let fresh = migration(
            r#"
async def upgrade(db):
    return 'CREATE TABLE "jobs" (id INT); ALTER TABLE "jobs" ADD "ref" VARCHAR(64) UNIQUE;'
"#,
        );
        assert!(RuleRegistry::new()
            .check(&fresh, &Config::default())
            .is_empty());
    }

    #[test]
    fn create_table_if_not_exists_does_not_exempt_later_index() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "CREATE TABLE IF NOT EXISTS jobs (id INT); CREATE INDEX jobs_state_idx ON jobs (state);"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "R001"));
    }

    #[test]
    fn generated_initial_table_with_comments_is_fresh() {
        let source = r#"
async def upgrade(db):
    return 'CREATE TABLE IF NOT EXISTS jobs (id INT); COMMENT ON TABLE jobs IS \'jobs\'; COMMENT ON COLUMN jobs.id IS \'id\'; CREATE INDEX jobs_id_idx ON jobs (id);'
"#;
        let initial =
            Migration::from_source(Path::new("migrations/models/0_20260903_init.py"), source)
                .unwrap();
        assert!(RuleRegistry::new()
            .check(&initial, &Config::default())
            .is_empty());
        let non_initial =
            Migration::from_source(Path::new("migrations/models/1_20260903_init.py"), source)
                .unwrap();
        assert!(RuleRegistry::new()
            .check(&non_initial, &Config::default())
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "R001"));
    }

    #[test]
    fn generated_later_table_is_fresh_but_handwritten_if_not_exists_is_not() {
        let sql = "async def upgrade(db):\n    return 'CREATE TABLE IF NOT EXISTS jobs (id INT); CREATE INDEX IF NOT EXISTS jobs_id_idx ON jobs (id);'\n";
        let generated = Migration::from_source(
            Path::new("migrations/models/2_20260903_jobs.py"),
            &format!("MODELS_STATE = {{}}\n{sql}"),
        )
        .unwrap();
        assert!(RuleRegistry::new()
            .check(&generated, &Config::default())
            .is_empty());
        let handwritten =
            Migration::from_source(Path::new("migrations/models/2_20260903_jobs.py"), sql).unwrap();
        assert!(RuleRegistry::new()
            .check(&handwritten, &Config::default())
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "R001"));
    }

    #[test]
    fn concurrent_aerich_index_requires_generated_transaction_setting() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "CREATE INDEX CONCURRENTLY jobs_state_idx ON jobs (state);"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R004"]
        );
    }

    #[test]
    fn unique_constraint_using_existing_index_is_safe() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "ALTER TABLE jobs ADD CONSTRAINT jobs_owner_unique UNIQUE USING INDEX jobs_owner_idx;"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
    }

    #[test]
    fn conditional_create_table_does_not_exempt_an_index() {
        let migration = migration(
            r#"
async def upgrade(db):
    if db:
        return "CREATE TABLE jobs (id INT);"
    return "CREATE INDEX jobs_state_idx ON jobs (state);"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "R001"));
    }

    #[test]
    fn unsupported_sql_invalidates_a_fresh_table_exemption() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "CREATE TABLE jobs (id INT); ALTER TABLE jobs OWNER TO app; CREATE INDEX jobs_state_idx ON jobs (state);"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "R001"));
    }

    #[test]
    fn identifiers_cannot_spoof_concurrent_or_not_valid_clauses() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "CREATE INDEX jobs_idx ON jobs (\"concurrently\"); ALTER TABLE jobs ADD CONSTRAINT jobs_check CHECK (\"not\" = \"valid\");"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.rule_id)
            .collect();
        assert_eq!(ids, vec!["R001", "R017"]);
    }

    #[test]
    fn type_change_is_a_warning() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "ALTER TABLE jobs ALTER COLUMN state TYPE VARCHAR(64);"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R015");
    }
}
