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
        self.extract_reachable_function("upgrade", &functions, &mut visited, true, &mut operations);

        Ok(Migration {
            path: path.to_path_buf(),
            framework: MigrationFramework::Aerich,
            is_non_atomic: false,
            operations,
            imports: vec![],
            class_span: None,
            line_ignores: self.extract_line_ignores(),
        })
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
        operations: &mut Vec<Operation>,
    ) {
        let Some(function) = functions.get(name) else {
            return;
        };
        if !visited.insert(name.to_string()) {
            return;
        }
        if let Some(body) = function.child_by_field_name("body") {
            self.extract_reachable_sql(body, functions, visited, is_unconditional, operations);
        }
    }

    fn extract_reachable_sql(
        &self,
        node: Node<'a>,
        functions: &BTreeMap<String, Node<'a>>,
        visited: &mut BTreeSet<String>,
        is_unconditional: bool,
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
                self.extract_sql(&sql, span, is_unconditional, operations);
            }
            return;
        }
        if node.kind() == "call" {
            self.extract_call_sql(node, functions, visited, is_unconditional, operations);
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
            self.extract_reachable_sql(child, functions, visited, is_unconditional, operations);
        }
    }

    fn extract_call_sql(
        &self,
        call: Node<'a>,
        functions: &BTreeMap<String, Node<'a>>,
        visited: &mut BTreeSet<String>,
        is_unconditional: bool,
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
                self.extract_sql(&sql, Span::from_node(&value), is_unconditional, operations);
            }
        }

        if functions.contains_key(function_name) {
            self.extract_reachable_function(
                function_name,
                functions,
                visited,
                is_unconditional,
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
        operations: &mut Vec<Operation>,
    ) {
        for statement in strip_sql_noise(sql).split(';') {
            let statement = statement.trim();
            let statement = if starts_with_words(statement, &["DO"]) {
                alter_table_in_do_block(statement).unwrap_or(statement)
            } else {
                statement
            };
            if let Some(operation) =
                self.extract_statement(statement, span, create_table_is_unconditional)
            {
                operations.push(operation);
            }
        }
    }

    fn extract_statement(
        &self,
        statement: &str,
        span: Span,
        create_table_is_unconditional: bool,
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
            in_autocommit_block: false,
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
                && !contains_words_in_order(statement, &["IF", "NOT", "EXISTS"]);
            return Some(operation(
                OperationType::CreateModel,
                OperationData::Model(ModelOperation {
                    name: table.name.clone(),
                    old_name: None,
                }),
                certain.then_some(table),
            ));
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
                        is_nullable: true,
                        has_default: false,
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
                        is_nullable: false,
                        has_default: false,
                        is_type_change: false,
                    }),
                }),
                Some(table),
            ));
        }
        if contains_words_in_order(statement, &["ADD", "COLUMN"]) {
            let column = identifier_after_keyword(statement, "COLUMN")?.name;
            return Some(operation(
                OperationType::AddField,
                OperationData::Field(FieldOperation {
                    model_name: table.name.clone(),
                    field_name: column,
                    old_name: None,
                    new_name: None,
                    field: Some(FieldInfo {
                        is_relation: contains_word(statement, "REFERENCES"),
                        is_nullable: !contains_words_in_order(statement, &["NOT", "NULL"]),
                        has_default: contains_word(statement, "DEFAULT"),
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
    fn concurrent_aerich_index_does_not_get_django_transaction_advice() {
        let migration = migration(
            r#"
async def upgrade(db):
    return "CREATE INDEX CONCURRENTLY jobs_state_idx ON jobs (state);"
"#,
        );
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
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
