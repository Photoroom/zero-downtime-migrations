//! Extraction of the deliberately small, static Alembic subset zdm supports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tree_sitter::Node;

use super::{
    ConstraintOperation, ConstraintType, FieldInfo, FieldOperation, IndexOperation, Migration,
    ModelOperation, Operation, OperationData, OperationType, RunSQLOperation, TableIdentity,
};
use crate::ast::extractor::{parse_ignore_directive, MigrationExtractor};
use crate::diagnostics::Span;
use crate::discovery::MigrationFramework;
use crate::error::Result;
use crate::parser::ParsedMigration;

/// Extracts direct `op.*` calls from an Alembic revision's `upgrade()` function.
pub(crate) struct AlembicMigrationExtractor<'a> {
    parsed: &'a ParsedMigration,
}

impl<'a> AlembicMigrationExtractor<'a> {
    pub(crate) fn new(parsed: &'a ParsedMigration) -> Self {
        Self { parsed }
    }

    pub(crate) fn extract(&self, path: &Path) -> Result<Migration> {
        let mut operations = Vec::new();
        if let Some(upgrade) = self.find_upgrade() {
            if let Some(body) = upgrade.child_by_field_name("body") {
                self.extract_operations(body, false, true, &mut operations);
            }
        }

        Ok(Migration {
            path: path.to_path_buf(),
            framework: MigrationFramework::Alembic,
            is_non_atomic: false,
            operations,
            imports: vec![],
            class_span: None,
            line_ignores: self.extract_line_ignores(),
        })
    }

    fn find_upgrade(&self) -> Option<Node<'a>> {
        let root = self.parsed.root_node();
        root.named_children(&mut root.walk()).find(|node| {
            node.kind() == "function_definition"
                && node
                    .child_by_field_name("name")
                    .is_some_and(|name| self.node_text(name) == "upgrade")
        })
    }

    fn extract_operations(
        &self,
        node: Node<'a>,
        in_autocommit_block: bool,
        create_table_is_unconditional: bool,
        out: &mut Vec<Operation>,
    ) {
        if matches!(node.kind(), "function_definition" | "lambda") {
            return;
        }
        if node.kind() == "call" {
            out.extend(self.extract_operation(
                node,
                in_autocommit_block,
                create_table_is_unconditional,
            ));
            return;
        }
        if node.kind() == "with_statement" {
            let in_autocommit_block = in_autocommit_block || self.is_autocommit_block(node);
            if let Some(body) = node.child_by_field_name("body") {
                self.extract_operations(
                    body,
                    in_autocommit_block,
                    create_table_is_unconditional,
                    out,
                );
            }
            return;
        }
        let create_table_is_unconditional = create_table_is_unconditional
            && !matches!(
                node.kind(),
                "if_statement"
                    | "for_statement"
                    | "while_statement"
                    | "try_statement"
                    | "match_statement"
            );
        for child in node.named_children(&mut node.walk()) {
            self.extract_operations(
                child,
                in_autocommit_block,
                create_table_is_unconditional,
                out,
            );
        }
    }

    fn is_autocommit_block(&self, node: Node<'a>) -> bool {
        let Some(clause) = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "with_clause")
        else {
            return false;
        };
        clause.named_children(&mut clause.walk()).any(|item| {
            let value = if item.kind() == "with_item" {
                item.named_child(0)
            } else {
                Some(item)
            };
            value.is_some_and(|value| {
                value.kind() == "call"
                    && value
                        .child_by_field_name("function")
                        .is_some_and(|function| {
                            self.node_text(function) == "op.get_context().autocommit_block"
                        })
            })
        })
    }

    fn extract_operation(
        &self,
        call: Node<'a>,
        in_autocommit_block: bool,
        create_table_is_unconditional: bool,
    ) -> Vec<Operation> {
        let Some(function) = call.child_by_field_name("function") else {
            return vec![];
        };
        let Some(name) = self.node_text(function).strip_prefix("op.") else {
            return vec![];
        };
        let Some(args) = call.child_by_field_name("arguments") else {
            return vec![];
        };
        let span = Span::from_node(&call);
        let op = |op_type, data, table_identity| Operation {
            op_type,
            span,
            data,
            table_identity,
            in_autocommit_block,
        };

        match name {
            "create_table" => self
                .nth_string(args, 0)
                .or_else(|| self.keyword_string(args, "table_name"))
                .map(|table| {
                    vec![op(
                        OperationType::CreateModel,
                        OperationData::Model(ModelOperation {
                            name: table.clone(),
                            old_name: None,
                        }),
                        (create_table_is_unconditional
                            && self.is_direct_statement(call)
                            && self.keyword_is_false_or_absent(args, "if_not_exists"))
                        .then(|| self.table_identity(args, table, "schema", None))
                        .flatten(),
                    )]
                }),
            "create_index" | "drop_index" => self
                .nth_string(args, 1)
                .or_else(|| self.keyword_string(args, "table_name"))
                .or_else(|| (name == "drop_index").then(String::new))
                .map(|table| {
                    let concurrent = self.keyword_is_true(args, "postgresql_concurrently");
                    let op_type = match (name, concurrent) {
                        ("create_index", true) => OperationType::AddIndexConcurrently,
                        ("create_index", false) => OperationType::AddIndex,
                        ("drop_index", true) => OperationType::RemoveIndexConcurrently,
                        _ => OperationType::RemoveIndex,
                    };
                    let schema = self.table_identity(args, table.clone(), "schema", None);
                    vec![op(
                        op_type,
                        OperationData::Index(IndexOperation { model_name: table }),
                        schema,
                    )]
                }),
            "drop_column" => self
                .nth_string(args, 0)
                .or_else(|| self.keyword_string(args, "table_name"))
                .zip(
                    self.nth_string(args, 1)
                        .or_else(|| self.keyword_string(args, "column_name")),
                )
                .map(|(table, column)| {
                    let table_identity = self.table_identity(args, table.clone(), "schema", None);
                    vec![op(
                        OperationType::RemoveField,
                        OperationData::Field(FieldOperation {
                            model_name: table,
                            field_name: column,
                            old_name: None,
                            new_name: None,
                            field: None,
                        }),
                        table_identity,
                    )]
                }),
            "add_column" => self
                .nth_string(args, 0)
                .or_else(|| self.keyword_string(args, "table_name"))
                .zip(
                    self.nth_value(args, 1)
                        .or_else(|| self.keyword_value(args, "column")),
                )
                .and_then(|(table, column)| {
                    self.column_field(column).map(|(field_name, field)| {
                        let table_identity =
                            self.table_identity(args, table.clone(), "schema", None);
                        let is_unique = field.is_unique;
                        let has_index = field.db_index;
                        let mut operations = vec![op(
                            OperationType::AddField,
                            OperationData::Field(FieldOperation {
                                model_name: table.clone(),
                                field_name,
                                old_name: None,
                                new_name: None,
                                field: Some(field),
                            }),
                            table_identity.clone(),
                        )];
                        if is_unique {
                            operations.push(op(
                                OperationType::AddConstraint,
                                OperationData::Constraint(ConstraintOperation {
                                    model_name: table.clone(),
                                    constraint_type: ConstraintType::Unique,
                                    not_valid: false,
                                    requires_state_only: false,
                                }),
                                table_identity.clone(),
                            ));
                        }
                        if has_index {
                            operations.push(op(
                                OperationType::AddIndex,
                                OperationData::Index(IndexOperation { model_name: table }),
                                table_identity,
                            ));
                        }
                        operations
                    })
                }),
            "alter_column" => self
                .nth_string(args, 0)
                .or_else(|| self.keyword_string(args, "table_name"))
                .zip(
                    self.nth_string(args, 1)
                        .or_else(|| self.keyword_string(args, "column_name")),
                )
                .map(|(table, column)| {
                    let table_identity =
                        self.table_identity(args, table.clone(), "schema", Some(11));
                    let mut operations = Vec::new();
                    if self.keyword_is_false(args, "nullable")
                        || self
                            .nth_value(args, 2)
                            .is_some_and(|value| self.value_is_false(value))
                    {
                        operations.push(op(
                            OperationType::AlterField,
                            OperationData::Field(FieldOperation {
                                model_name: table.clone(),
                                field_name: column.clone(),
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
                            table_identity.clone(),
                        ));
                    }
                    if self
                        .keyword_value(args, "type_")
                        .or_else(|| self.nth_value(args, 6))
                        .is_some_and(|value| self.node_text(value).trim() != "None")
                    {
                        operations.push(op(
                            OperationType::AlterField,
                            OperationData::Field(FieldOperation {
                                model_name: table.clone(),
                                field_name: column.clone(),
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
                            table_identity.clone(),
                        ));
                    }
                    if let Some(new_name) = self
                        .keyword_string(args, "new_column_name")
                        .or_else(|| self.nth_string(args, 5))
                    {
                        operations.push(op(
                            OperationType::RenameField,
                            OperationData::Field(FieldOperation {
                                model_name: table,
                                field_name: column.clone(),
                                old_name: Some(column),
                                new_name: Some(new_name),
                                field: None,
                            }),
                            table_identity,
                        ));
                    }
                    operations
                }),
            "create_unique_constraint" => self
                .nth_string(args, 1)
                .or_else(|| self.keyword_string(args, "table_name"))
                .map(|table| {
                    let table_identity = self.table_identity(args, table.clone(), "schema", None);
                    vec![op(
                        OperationType::AddConstraint,
                        OperationData::Constraint(ConstraintOperation {
                            model_name: table,
                            constraint_type: ConstraintType::Unique,
                            not_valid: false,
                            requires_state_only: false,
                        }),
                        table_identity,
                    )]
                }),
            "rename_table" => self
                .nth_string(args, 0)
                .or_else(|| self.keyword_string(args, "old_table_name"))
                .zip(
                    self.nth_string(args, 1)
                        .or_else(|| self.keyword_string(args, "new_table_name")),
                )
                .map(|(old_name, name)| {
                    let table_identity = self.table_identity(args, name.clone(), "schema", None);
                    vec![op(
                        OperationType::RenameModel,
                        OperationData::Model(ModelOperation {
                            name,
                            old_name: Some(old_name),
                        }),
                        table_identity,
                    )]
                }),
            "drop_table" => self
                .nth_string(args, 0)
                .or_else(|| self.keyword_string(args, "table_name"))
                .map(|name| {
                    let table_identity = self.table_identity(args, name.clone(), "schema", None);
                    vec![op(
                        OperationType::DeleteModel,
                        OperationData::Model(ModelOperation {
                            name,
                            old_name: None,
                        }),
                        table_identity,
                    )]
                }),
            "create_foreign_key" | "create_check_constraint" | "create_exclude_constraint" => {
                let constraint_type = match name {
                    "create_foreign_key" => ConstraintType::ForeignKey,
                    "create_check_constraint" => ConstraintType::Check,
                    _ => ConstraintType::Exclusion,
                };
                self.nth_string(args, 1)
                    .or_else(|| {
                        self.keyword_string(
                            args,
                            if name == "create_foreign_key" {
                                "source_table"
                            } else {
                                "table_name"
                            },
                        )
                    })
                    .map(|table| {
                        let schema = if name == "create_foreign_key" {
                            "source_schema"
                        } else {
                            "schema"
                        };
                        let table_identity = self.table_identity(args, table.clone(), schema, None);
                        vec![op(
                            OperationType::AddConstraint,
                            OperationData::Constraint(ConstraintOperation {
                                model_name: table,
                                constraint_type,
                                not_valid: self.keyword_is_true(args, "postgresql_not_valid"),
                                requires_state_only: false,
                            }),
                            table_identity,
                        )]
                    })
            }
            "execute" => self.static_sql(args).map(|sql| {
                vec![op(
                    OperationType::ExecuteSql,
                    OperationData::RunSQL(RunSQLOperation {
                        sql,
                        reverse_sql: None,
                    }),
                    None,
                )]
            }),
            _ => None,
        }
        .unwrap_or_default()
    }

    fn table_identity(
        &self,
        args: Node<'a>,
        name: String,
        schema_name: &str,
        schema_position: Option<usize>,
    ) -> Option<TableIdentity> {
        if self.has_dictionary_splat(args) {
            return None;
        }
        match self.keyword_value(args, schema_name).or_else(|| {
            schema_position
                .and_then(|position| self.positional_values(args).into_iter().nth(position))
        }) {
            Some(schema) if self.node_text(schema).trim() == "None" => {
                Some(TableIdentity { schema: None, name })
            }
            Some(schema) => self.static_string(schema).map(|schema| TableIdentity {
                schema: Some(schema),
                name,
            }),
            None => Some(TableIdentity { schema: None, name }),
        }
    }

    fn is_direct_statement(&self, call: Node<'a>) -> bool {
        call.parent()
            .is_some_and(|parent| parent.kind() == "expression_statement")
    }

    fn positional_values(&self, args: Node<'a>) -> Vec<Node<'a>> {
        args.named_children(&mut args.walk())
            .filter(|child| child.kind() != "keyword_argument")
            .collect()
    }

    fn nth_string(&self, args: Node<'a>, n: usize) -> Option<String> {
        self.positional_values(args)
            .into_iter()
            .nth(n)
            .and_then(|value| self.static_string(value))
    }

    fn nth_value(&self, args: Node<'a>, n: usize) -> Option<Node<'a>> {
        self.positional_values(args).into_iter().nth(n)
    }

    fn column_field(&self, column: Node<'a>) -> Option<(String, FieldInfo)> {
        let function = column.child_by_field_name("function")?;
        if !matches!(self.node_text(function), "Column" | "sa.Column") {
            return None;
        }
        let args = column.child_by_field_name("arguments")?;
        let field_name = self.nth_string(args, 0)?;
        let is_relation = self.positional_values(args).into_iter().any(|value| {
            value
                .child_by_field_name("function")
                .is_some_and(|function| {
                    matches!(
                        self.node_text(function),
                        "ForeignKey" | "sa.ForeignKey" | "sqlalchemy.ForeignKey"
                    )
                })
        });
        let is_nullable =
            !self.keyword_is_false(args, "nullable") && !self.keyword_is_true(args, "primary_key");
        let has_default = self
            .keyword_value(args, "server_default")
            .is_some_and(|value| self.node_text(value).trim() != "None");
        Some((
            field_name,
            FieldInfo {
                is_relation,
                is_foreign_key: is_relation,
                db_constraint: true,
                db_index: self.keyword_is_true(args, "index"),
                db_index_disabled: false,
                is_unique: self.keyword_is_true(args, "unique"),
                is_nullable,
                has_default: false,
                has_db_default: has_default,
                is_type_change: false,
            },
        ))
    }

    fn keyword_value(&self, args: Node<'a>, name: &str) -> Option<Node<'a>> {
        args.named_children(&mut args.walk()).find_map(|child| {
            (child.kind() == "keyword_argument"
                && child
                    .child_by_field_name("name")
                    .is_some_and(|key| self.node_text(key) == name))
            .then(|| child.child_by_field_name("value"))
            .flatten()
        })
    }

    fn keyword_string(&self, args: Node<'a>, name: &str) -> Option<String> {
        self.keyword_value(args, name)
            .and_then(|value| self.static_string(value))
    }

    fn keyword_is_true(&self, args: Node<'a>, name: &str) -> bool {
        self.keyword_value(args, name)
            .is_some_and(|value| self.node_text(value).trim() == "True")
    }

    fn keyword_is_false(&self, args: Node<'a>, name: &str) -> bool {
        self.keyword_value(args, name)
            .is_some_and(|value| self.value_is_false(value))
    }

    fn value_is_false(&self, value: Node<'a>) -> bool {
        self.node_text(value).trim() == "False"
    }

    fn keyword_is_false_or_absent(&self, args: Node<'a>, name: &str) -> bool {
        self.keyword_value(args, name)
            .is_none_or(|value| matches!(self.node_text(value).trim(), "False" | "None"))
    }

    fn has_dictionary_splat(&self, args: Node<'a>) -> bool {
        args.named_children(&mut args.walk())
            .any(|child| child.kind() == "dictionary_splat")
    }

    fn static_sql(&self, args: Node<'a>) -> Option<String> {
        self.positional_values(args)
            .into_iter()
            .next()
            .and_then(|value| self.static_string(value))
            .or_else(|| self.keyword_string(args, "sqltext"))
    }

    fn static_string(&self, node: Node<'a>) -> Option<String> {
        matches!(node.kind(), "string" | "concatenated_string")
            .then(|| MigrationExtractor::new(self.parsed).extract_string_value(node))
            .filter(|value| !value.is_empty())
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::ast::{Migration, OperationType};
    use crate::config::Config;
    use crate::discovery::MigrationFramework;
    use crate::rules::RuleRegistry;

    const UNSAFE: &str = r#"
from alembic import op


def upgrade():
    op.create_index("jobs_state_idx", "jobs", ["state"])
    op.drop_index("old_jobs_idx", table_name="jobs")
    op.create_foreign_key("jobs_owner_fk", "jobs", "owners", ["owner_id"], ["id"])
    op.create_check_constraint("jobs_state_check", "jobs", "state <> ''")
    op.create_exclude_constraint("jobs_overlap_excl", "jobs", "period WITH &&")
    op.alter_column("jobs", "owner_id", nullable=False)
    op.alter_column("jobs", "state", new_column_name="status")
    op.drop_column("jobs", "legacy_state")
    op.execute("CREATE INDEX jobs_created_idx ON jobs (created_at)")
    op.create_index("jobs_owner_idx", "jobs", ["owner_id"], postgresql_concurrently=True)


def downgrade():
    op.create_index("ignored_idx", "jobs", ["ignored"])
"#;

    const SAFE_NEW_TABLE_AND_AUTOCOMMIT: &str = r#"
from alembic import op


def upgrade():
    op.create_table("jobs")
    op.create_index("jobs_state_idx", "jobs", ["state"])
    op.create_check_constraint("jobs_state_check", "jobs", "state <> ''")
    with op.get_context().autocommit_block():
        op.create_index("jobs_owner_idx", "jobs", ["owner_id"], postgresql_concurrently=True)
        op.execute("CREATE INDEX CONCURRENTLY jobs_created_idx ON jobs (created_at)")
"#;

    const KEYWORD_UNSAFE: &str = r#"
from alembic import op


def upgrade():
    op.drop_column(table_name="jobs", column_name="legacy_state")
    op.alter_column(table_name="jobs", column_name="state", nullable=False)
    op.alter_column(table_name="jobs", column_name="state", new_column_name="status")
    op.create_foreign_key(constraint_name="jobs_owner_fk", source_table="jobs", remote_table="owners", local_cols=["owner_id"], remote_cols=["id"])
    op.create_check_constraint(constraint_name="jobs_state_check", table_name="jobs", condition="state <> ''")
    op.create_exclude_constraint(constraint_name="jobs_overlap_excl", table_name="jobs", where="period WITH &&")
"#;

    const CONDITIONAL_AUTOCOMMIT: &str = r#"
from alembic import op


def upgrade():
    with (op.get_context().autocommit_block() if enabled else nullcontext()):
        op.create_index("jobs_owner_idx", "jobs", ["owner_id"], postgresql_concurrently=True)
"#;

    const ESCAPED_CONCATENATED_SQL: &str = r#"
from alembic import op


def upgrade():
    op.execute("CREATE\n" "INDEX jobs_created_idx ON jobs (created_at)")
"#;

    const OPTIONAL_INDEX_AND_KEYWORD_SQL: &str = r#"
from alembic import op


def upgrade():
    op.drop_index("old_jobs_idx")
    op.execute(sqltext="CREATE INDEX jobs_created_idx ON jobs (created_at)")
"#;

    const DEFERRED_SCOPES: &str = r#"
from alembic import op


def upgrade():
    def unused():
        op.create_index("unused_idx", "jobs", ["state"])

    class Evaluated:
        def unused_method():
            op.drop_column("jobs", "legacy_state")

    lambda: op.create_check_constraint("unused_check", "jobs", "state <> ''")
"#;

    const CLASS_BODY_OPERATION: &str = r#"
from alembic import op


def upgrade():
    class Evaluated:
        op.create_index("class_idx", "jobs", ["state"])
"#;

    const CONDITIONAL_CREATE_TABLE: &str = r#"
from alembic import op


def upgrade():
    if enabled:
        op.create_table("jobs")
    op.create_index("jobs_idx", "jobs", ["id"])
"#;

    const CONDITIONAL_EXPRESSION_CREATE_TABLE: &str = r#"
from alembic import op


def upgrade():
    op.create_table("jobs") if enabled else None
    op.create_index("jobs_idx", "jobs", ["id"])
"#;

    const IF_NOT_EXISTS_CREATE_TABLE: &str = r#"
from alembic import op


def upgrade():
    op.create_table("jobs", if_not_exists=True)
    op.create_index("jobs_idx", "jobs", ["id"])
"#;

    const CREATE_TABLE_KEYWORD_SPLAT: &str = r#"
from alembic import op


def upgrade():
    op.create_table("jobs", **{"schema": "archive"})
    op.create_index("jobs_idx", "jobs", ["id"])
"#;

    const TARGET_KEYWORD_SPLAT: &str = r#"
from alembic import op


def upgrade():
    op.create_table("jobs")
    op.create_index("jobs_idx", "jobs", ["id"], **{"schema": "archive"})
"#;

    const SQL_BETWEEN_UNQUALIFIED_TABLE_OPERATIONS: &str = r#"
from alembic import op


def upgrade():
    op.create_table("jobs")
    op.execute("SELECT 1")
    op.create_index("jobs_idx", "jobs", ["id"])
"#;

    const SQL_BETWEEN_QUALIFIED_TABLE_OPERATIONS: &str = r#"
from alembic import op


def upgrade():
    op.create_table("jobs", schema="archive")
    op.execute("SELECT 1")
    op.create_index("jobs_idx", "jobs", ["id"], schema="archive")
"#;

    const MULTI_CONTEXT_AUTOCOMMIT: &str = r#"
from contextlib import nullcontext
from alembic import op


def upgrade():
    with nullcontext(), op.get_context().autocommit_block():
        op.create_index("jobs_state_idx", "jobs", ["state"], postgresql_concurrently=True)
"#;

    const SCHEMA_AND_CASE_IDENTITIES: &str = r#"
from alembic import op


def upgrade():
    op.create_table("users", schema="new")
    op.create_index("new_users_idx", "users", ["id"], schema="new")
    op.create_index("public_users_idx", "users", ["id"], schema="public")
    op.create_table("Users")
    op.create_index("lowercase_users_idx", "users", ["id"])
"#;

    const DYNAMIC_SCHEMA: &str = r#"
from alembic import op


def upgrade():
    op.create_table("users", schema=target_schema)
    op.create_index("users_idx", "users", ["id"], schema=target_schema)
"#;

    const EXPLICIT_DEFAULT_SCHEMA: &str = r#"
from alembic import op


def upgrade():
    op.create_table("users", schema=None)
    op.create_index("users_idx", "users", ["id"], schema=None)
"#;

    const NOT_VALID_CONSTRAINTS: &str = r#"
from alembic import op


def upgrade():
    op.create_foreign_key("safe_fk", "jobs", "owners", ["owner_id"], ["id"], postgresql_not_valid=True)
    op.create_check_constraint("safe_check", "jobs", "state <> ''", postgresql_not_valid=True)
    op.create_foreign_key("unsafe_fk", "jobs", "owners", ["owner_id"], ["id"], postgresql_not_valid=False)
    op.create_check_constraint("dynamic_check", "jobs", "state <> ''", postgresql_not_valid=not_valid)
"#;

    const ADD_COLUMNS: &str = r#"
from alembic import op
import sqlalchemy as sa


def upgrade():
    op.add_column("jobs", sa.Column("required", sa.String(), nullable=False))
    op.add_column("jobs", sa.Column("defaulted", sa.String(), nullable=False, server_default="queued"))
    op.add_column("jobs", sa.Column("none_default", sa.String(), nullable=False, server_default=None))
    op.add_column("jobs", sa.Column("owner_id", sa.Integer(), sa.ForeignKey("owners.id"), nullable=False))
    op.create_table("new_jobs")
    op.add_column("new_jobs", sa.Column("required", sa.String(), nullable=False))
    op.add_column("jobs", sa.Column("id", sa.Integer(), primary_key=True))
    op.add_column("jobs", sa.Column("plain", sa.String(), nullable=True))
"#;

    const ADD_COLUMN_FLAGS: &str = r#"
from alembic import op
import sqlalchemy as sa


def upgrade():
    op.add_column("jobs", sa.Column("code", sa.String(), unique=True))
    op.add_column("jobs", sa.Column("slug", sa.String(), index=True))
"#;

    const SAFE_ADD_COLUMN: &str = r#"
from alembic import op
import sqlalchemy as sa


def upgrade():
    op.add_column("jobs", sa.Column("label", sa.String()))
"#;

    const QUALIFIED_FOREIGN_KEY: &str = r#"
from alembic import op
import sqlalchemy
import sqlalchemy as sa


def upgrade():
    op.add_column("jobs", sa.Column("owner_id", sa.Integer(), sqlalchemy.ForeignKey("owners.id")))
"#;

    const UNIQUE_CONSTRAINTS_AND_TYPE_CHANGES: &str = r#"
from alembic import op
import sqlalchemy as sa


def upgrade():
    op.create_unique_constraint("jobs_code_key", "jobs", ["code"])
    op.create_unique_constraint(
        constraint_name="archive_jobs_code_key",
        table_name="jobs",
        columns=["code"],
        schema="archive",
    )
    op.create_table("new_jobs")
    op.create_unique_constraint("new_jobs_code_key", "new_jobs", ["code"])
    op.alter_column("jobs", "state", type_=sa.String())
    op.alter_column(table_name="jobs", column_name="ignored", type_=None)
    op.alter_column(
        "archive_jobs", "old_code", type_=sa.String(), new_column_name="new_code", schema="archive"
    )
    op.alter_column(
        "archive_jobs", "legacy_code", None, None, None, None, sa.String(), None, None, None, None, "archive"
    )
"#;

    const TABLE_RENAMES_AND_DROPS: &str = r#"
from alembic import op


def upgrade():
    op.rename_table("jobs", "archived_jobs")
    op.drop_table("legacy_jobs")
    op.create_table("events", schema="archive")
    op.rename_table("events", "archived_events", schema="archive")
    op.create_index("archived_events_idx", "archived_events", ["id"], schema="archive")
    op.drop_table("archived_events", schema="archive")
    op.drop_table("events", schema="archive")
"#;

    fn migration(source: &str) -> Migration {
        Migration::from_source(Path::new("alembic/versions/20260809_jobs.py"), source).unwrap()
    }

    #[test]
    fn extracts_direct_upgrade_operations_and_ignores_downgrade() {
        let migration = migration(UNSAFE);
        assert_eq!(migration.framework, MigrationFramework::Alembic);
        assert_eq!(
            migration
                .operations
                .iter()
                .map(|op| op.op_type)
                .collect::<Vec<_>>(),
            vec![
                OperationType::AddIndex,
                OperationType::RemoveIndex,
                OperationType::AddConstraint,
                OperationType::AddConstraint,
                OperationType::AddConstraint,
                OperationType::AlterField,
                OperationType::RenameField,
                OperationType::RemoveField,
                OperationType::ExecuteSql,
                OperationType::AddIndexConcurrently,
            ]
        );
    }

    #[test]
    fn existing_table_operations_reuse_the_safety_rules() {
        let migration = migration(UNSAFE);
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.rule_id)
            .collect();
        assert_eq!(
            ids,
            vec!["R001", "R003", "R004", "R005", "R011", "R015", "R016", "R017", "R017", "R017"]
        );
        assert!(diagnostics
            .iter()
            .all(|diagnostic| diagnostic.path.ends_with("20260809_jobs.py")));
    }

    #[test]
    fn new_table_and_canonical_autocommit_boundary_are_exempt() {
        let migration = migration(SAFE_NEW_TABLE_AND_AUTOCOMMIT);
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn keyword_arguments_cover_the_same_unsafe_operations() {
        let migration = migration(KEYWORD_UNSAFE);
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.rule_id)
            .collect();
        assert_eq!(ids, vec!["R005", "R011", "R015", "R017", "R017", "R017"]);
    }

    #[test]
    fn conditional_autocommit_expression_is_not_accepted_as_a_boundary() {
        let migration = migration(CONDITIONAL_AUTOCOMMIT);
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
    fn static_concatenated_sql_uses_the_shared_literal_decoder() {
        let migration = migration(ESCAPED_CONCATENATED_SQL);
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R003"]
        );
    }

    #[test]
    fn optional_index_table_and_keyword_sql_are_checked() {
        let migration = migration(OPTIONAL_INDEX_AND_KEYWORD_SQL);
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R003", "R016"]
        );
    }

    #[test]
    fn ignores_operations_in_deferred_scopes() {
        assert!(migration(DEFERRED_SCOPES).operations.is_empty());
    }

    #[test]
    fn evaluates_operations_in_class_bodies() {
        let diagnostics =
            RuleRegistry::new().check(&migration(CLASS_BODY_OPERATION), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn conditional_table_creation_does_not_receive_a_fresh_table_exemption() {
        let diagnostics =
            RuleRegistry::new().check(&migration(CONDITIONAL_CREATE_TABLE), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn conditional_expression_table_creation_does_not_receive_a_fresh_table_exemption() {
        let diagnostics = RuleRegistry::new().check(
            &migration(CONDITIONAL_EXPRESSION_CREATE_TABLE),
            &Config::default(),
        );
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn if_not_exists_table_creation_does_not_receive_a_fresh_table_exemption() {
        let diagnostics =
            RuleRegistry::new().check(&migration(IF_NOT_EXISTS_CREATE_TABLE), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn create_table_keyword_splat_does_not_receive_a_fresh_table_exemption() {
        let diagnostics =
            RuleRegistry::new().check(&migration(CREATE_TABLE_KEYWORD_SPLAT), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn target_keyword_splat_does_not_receive_a_fresh_table_exemption() {
        let diagnostics =
            RuleRegistry::new().check(&migration(TARGET_KEYWORD_SPLAT), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn sql_invalidates_unqualified_fresh_tables() {
        let diagnostics = RuleRegistry::new().check(
            &migration(SQL_BETWEEN_UNQUALIFIED_TABLE_OPERATIONS),
            &Config::default(),
        );
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn sql_invalidates_qualified_fresh_tables() {
        let diagnostics = RuleRegistry::new().check(
            &migration(SQL_BETWEEN_QUALIFIED_TABLE_OPERATIONS),
            &Config::default(),
        );
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn accepts_autocommit_block_after_another_context_manager() {
        let diagnostics =
            RuleRegistry::new().check(&migration(MULTI_CONTEXT_AUTOCOMMIT), &Config::default());
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn fresh_table_exemption_preserves_alembic_schema_and_case() {
        let diagnostics =
            RuleRegistry::new().check(&migration(SCHEMA_AND_CASE_IDENTITIES), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001", "R001"],
        );
    }

    #[test]
    fn dynamic_schema_does_not_receive_a_fresh_table_exemption() {
        let diagnostics = RuleRegistry::new().check(&migration(DYNAMIC_SCHEMA), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001"],
        );
    }

    #[test]
    fn explicit_default_schema_receives_a_fresh_table_exemption() {
        let diagnostics =
            RuleRegistry::new().check(&migration(EXPLICIT_DEFAULT_SCHEMA), &Config::default());
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:#?}"
        );
    }

    #[test]
    fn not_valid_check_and_foreign_key_constraints_are_safe() {
        let diagnostics =
            RuleRegistry::new().check(&migration(NOT_VALID_CONSTRAINTS), &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R017", "R017"],
        );
    }

    #[test]
    fn add_column_reuses_add_field_rules_without_column_constraints() {
        let migration = migration(ADD_COLUMNS);
        assert_eq!(
            migration
                .operations
                .iter()
                .map(|op| op.op_type)
                .collect::<Vec<_>>(),
            vec![
                OperationType::AddField,
                OperationType::AddField,
                OperationType::AddField,
                OperationType::AddField,
                OperationType::CreateModel,
                OperationType::AddField,
                OperationType::AddField,
                OperationType::AddField,
            ],
        );

        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R006", "R010", "R010", "R010", "R010"],
        );
    }

    #[test]
    fn add_column_unique_and_index_flags_reuse_their_safety_rules() {
        let migration = migration(ADD_COLUMN_FLAGS);
        assert_eq!(
            migration
                .operations
                .iter()
                .map(|op| op.op_type)
                .collect::<Vec<_>>(),
            vec![
                OperationType::AddField,
                OperationType::AddConstraint,
                OperationType::AddField,
                OperationType::AddIndex,
            ],
        );
        assert_eq!(
            RuleRegistry::new()
                .check(&migration, &Config::default())
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R001", "R002"],
        );
    }

    #[test]
    fn add_column_without_index_or_unique_is_safe() {
        assert!(RuleRegistry::new()
            .check(&migration(SAFE_ADD_COLUMN), &Config::default())
            .is_empty());
    }

    #[test]
    fn add_column_recognises_qualified_foreign_keys() {
        assert_eq!(
            RuleRegistry::new()
                .check(&migration(QUALIFIED_FOREIGN_KEY), &Config::default())
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R006"],
        );
    }

    #[test]
    fn unique_constraints_and_type_changes_reuse_the_safety_rules() {
        let migration = migration(UNIQUE_CONSTRAINTS_AND_TYPE_CHANGES);
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R002", "R002", "R011", "R015", "R015", "R015"],
        );
        assert_eq!(
            migration
                .operations
                .last()
                .and_then(|op| op.table_identity.as_ref())
                .map(|table| table.schema.as_deref()),
            Some(Some("archive")),
        );
        assert!(diagnostics[0]
            .help
            .as_ref()
            .is_some_and(|help| help.contains("UNIQUE INDEX CONCURRENTLY")));
    }

    #[test]
    fn table_renames_and_drops_reuse_r019_with_schema_aware_freshness() {
        let migration = migration(TABLE_RENAMES_AND_DROPS);
        let diagnostics = RuleRegistry::new().check(&migration, &Config::default());
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.rule_id)
                .collect::<Vec<_>>(),
            vec!["R019", "R019", "R019"],
        );
    }
}
