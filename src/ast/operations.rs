//! Django migration operation types.

use crate::diagnostics::Span;

/// A Django migration operation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Operation {
    /// The type of operation.
    pub op_type: OperationType,
    /// The span of the operation in the source.
    pub span: Span,
    /// Operation-specific data.
    pub data: OperationData,
    /// Exact Alembic table identity when this operation targets a statically
    /// known table. Django uses model names instead.
    pub table_identity: Option<TableIdentity>,
    /// Whether an Alembic operation is nested in its required
    /// `op.get_context().autocommit_block()` boundary. Django operations leave
    /// this false and use migration-level `atomic = False` instead.
    pub in_autocommit_block: bool,
}

/// A schema-qualified Alembic table identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableIdentity {
    pub schema: Option<String>,
    pub name: String,
}

impl Operation {
    /// The target model this operation acts on, when it has a single one
    /// (index, field, and constraint operations). Returns `None` for
    /// operations without a target model — `CreateModel`, `RunSQL`,
    /// `RunPython`, and `SeparateDatabaseAndState`. Used by the
    /// created-in-this-migration exemption shared across several rules.
    pub fn model_name(&self) -> Option<&str> {
        match &self.data {
            OperationData::Index(op) => Some(&op.model_name),
            OperationData::Field(op) => Some(&op.model_name),
            OperationData::Constraint(op) => Some(&op.model_name),
            _ => None,
        }
    }
}

/// The type of a Django migration operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperationType {
    // Index operations
    AddIndex,
    AddIndexConcurrently,
    RemoveIndex,
    RemoveIndexConcurrently,

    // Model operations
    CreateModel,
    DeleteModel,
    RenameModel,

    // Field operations
    AddField,
    RemoveField,
    AlterField,
    RenameField,

    // Constraint operations
    AddConstraint,
    RemoveConstraint,

    // Data operations
    RunSQL,
    /// A static SQL statement passed to Alembic's `op.execute`.
    ExecuteSql,
    RunPython,

    // Special operations
    SeparateDatabaseAndState,
    AlterModelOptions,
    AlterModelManagers,
    AlterModelTable,
    AlterUniqueTogether,
    AlterIndexTogether,
    AlterOrderWithRespectTo,

    // Unknown operation
    Unknown,
}

impl OperationType {
    /// Parse an operation type from its string name.
    pub fn from_name(name: &str) -> Self {
        match name {
            "AddIndex" => Self::AddIndex,
            "AddIndexConcurrently" => Self::AddIndexConcurrently,
            "RemoveIndex" => Self::RemoveIndex,
            "RemoveIndexConcurrently" => Self::RemoveIndexConcurrently,
            "CreateModel" => Self::CreateModel,
            "DeleteModel" => Self::DeleteModel,
            "RenameModel" => Self::RenameModel,
            "AddField" => Self::AddField,
            "RemoveField" => Self::RemoveField,
            "AlterField" => Self::AlterField,
            "RenameField" => Self::RenameField,
            "AddConstraint" => Self::AddConstraint,
            "RemoveConstraint" => Self::RemoveConstraint,
            "RunSQL" => Self::RunSQL,
            "RunPython" => Self::RunPython,
            "SeparateDatabaseAndState" => Self::SeparateDatabaseAndState,
            "AlterModelOptions" => Self::AlterModelOptions,
            "AlterModelManagers" => Self::AlterModelManagers,
            "AlterModelTable" => Self::AlterModelTable,
            "AlterUniqueTogether" => Self::AlterUniqueTogether,
            "AlterIndexTogether" => Self::AlterIndexTogether,
            "AlterOrderWithRespectTo" => Self::AlterOrderWithRespectTo,
            _ => Self::Unknown,
        }
    }

    /// Check if this is an index operation.
    pub fn is_index_operation(&self) -> bool {
        matches!(
            self,
            Self::AddIndex
                | Self::AddIndexConcurrently
                | Self::RemoveIndex
                | Self::RemoveIndexConcurrently
        )
    }

    /// Check if this is a concurrent operation.
    pub fn is_concurrent(&self) -> bool {
        matches!(
            self,
            Self::AddIndexConcurrently | Self::RemoveIndexConcurrently
        )
    }
}

/// Operation-specific data.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OperationData {
    /// Index operation data.
    Index(IndexOperation),
    /// Model operation data.
    Model(ModelOperation),
    /// Field operation data.
    Field(FieldOperation),
    /// Constraint operation data.
    Constraint(ConstraintOperation),
    /// RunSQL operation data.
    RunSQL(RunSQLOperation),
    /// RunPython operation data.
    RunPython(RunPythonOperation),
    /// SeparateDatabaseAndState data.
    SeparateDatabaseAndState(SeparateDatabaseAndStateOperation),
    /// No additional data.
    Empty,
}

/// Data for index operations (AddIndex, RemoveIndex, etc.).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IndexOperation {
    /// The model name (lowercase).
    pub model_name: String,
}

/// Data for model operations (CreateModel, DeleteModel, etc.).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ModelOperation {
    /// The model name.
    pub name: String,
    /// Old name (for RenameModel).
    pub old_name: Option<String>,
}

/// Data for field operations (AddField, RemoveField, etc.).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FieldOperation {
    /// The model name.
    pub model_name: String,
    /// The field name.
    pub field_name: String,
    /// Old name (for RenameField).
    pub old_name: Option<String>,
    /// New name (for RenameField).
    pub new_name: Option<String>,
    /// Field info (for AddField, AlterField).
    pub field: Option<FieldInfo>,
}

/// Field information for AddField/AlterField.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FieldInfo {
    /// Whether the field is a `ForeignKey` or `OneToOneField`.
    pub is_relation: bool,
    /// Whether the field is specifically a `ForeignKey`.
    pub is_foreign_key: bool,
    /// Whether Django will create the database constraint.
    pub db_constraint: bool,
    /// Whether Django explicitly creates a database index.
    pub db_index: bool,
    /// Whether Django explicitly disables the implicit relation index.
    pub db_index_disabled: bool,
    /// Whether the field is unique.
    pub is_unique: bool,
    /// Whether the field is nullable.
    pub is_nullable: bool,
    /// Whether the field has a default value.
    pub has_default: bool,
    /// Whether the field has a database default value.
    pub has_db_default: bool,
    /// Whether the operation changes an existing column's type.
    pub is_type_change: bool,
}

/// Data for constraint operations.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConstraintOperation {
    /// The model name.
    pub model_name: String,
    /// The constraint type.
    pub constraint_type: ConstraintType,
    /// Whether Alembic creates this constraint as `NOT VALID`.
    pub not_valid: bool,
}

/// Type of database constraint added via `migrations.AddConstraint`.
///
/// Django ships three constraint classes (`UniqueConstraint`,
/// `CheckConstraint`, `ExclusionConstraint`); foreign keys are added via
/// `AddField`, not `AddConstraint`, so they don't appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConstraintType {
    Unique,
    ForeignKey,
    Check,
    Exclusion,
    Unknown,
}

/// Data for RunSQL operations.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunSQLOperation {
    /// The forward SQL statement.
    pub sql: String,
    /// The reverse SQL statement, if provided.
    pub reverse_sql: Option<String>,
}

impl RunSQLOperation {
    /// Check if the SQL contains a `CREATE INDEX` or `CREATE UNIQUE INDEX`
    /// statement. Comments (`-- ...`, `/* ... */`) and single-quoted string
    /// literals are stripped before the substring search so SQL like
    /// `"-- about CREATE INDEX"` or `"'CREATE INDEX'"` does not match.
    pub fn contains_create_index(&self) -> bool {
        strip_sql_noise(&self.sql)
            .split(';')
            .any(sql_statement_contains_create_index)
    }

    /// Check if the SQL contains a `DROP INDEX` statement, ignoring
    /// comments and string literals.
    pub fn contains_drop_index(&self) -> bool {
        strip_sql_noise(&self.sql)
            .split(';')
            .any(sql_statement_contains_drop_index)
    }
}

pub(crate) fn sql_statement_contains_create_index(statement: &str) -> bool {
    let tokens = sql_tokens(statement);
    tokens.first().is_some_and(|t| t == "CREATE")
        && (tokens.get(1).is_some_and(|t| t == "INDEX")
            || (tokens.get(1).is_some_and(|t| t == "UNIQUE")
                && tokens.get(2).is_some_and(|t| t == "INDEX")))
        || tokens
            .first()
            .is_some_and(|t| compact_sql_token_starts_with(t, &["CREATE", "INDEX"]))
        || tokens
            .first()
            .is_some_and(|t| compact_sql_token_starts_with(t, &["CREATE", "UNIQUE", "INDEX"]))
}

pub(crate) fn sql_statement_contains_drop_index(statement: &str) -> bool {
    let tokens = sql_tokens(statement);
    tokens.first().is_some_and(|t| t == "DROP") && tokens.get(1).is_some_and(|t| t == "INDEX")
        || tokens
            .first()
            .is_some_and(|t| compact_sql_token_starts_with(t, &["DROP", "INDEX"]))
}

pub(crate) fn sql_statement_contains_reindex(statement: &str) -> bool {
    sql_tokens(statement)
        .first()
        .is_some_and(|t| t == "REINDEX")
}

pub(crate) fn sql_statement_contains_concurrently(statement: &str) -> bool {
    sql_tokens(statement).iter().any(|t| t == "CONCURRENTLY")
}

fn compact_sql_token_starts_with(token: &str, keywords: &[&str]) -> bool {
    if keywords.is_empty() {
        return false;
    }
    let expected = keywords.join("");
    token.starts_with(&expected)
}

fn sql_tokens(statement: &str) -> Vec<String> {
    // Split on any non-identifier byte. Python escapes (`\n`, `\x20`, …) are
    // already resolved before SQL ever reaches here: the extractor decodes
    // them in non-raw strings (so `'CREATE\nINDEX'` arrives as real
    // whitespace and splits cleanly) and preserves the backslash in raw
    // strings (so `r'CREATE\nINDEX'` keeps the `\`, which is itself a
    // separator and breaks the keyword run). Neither path can produce a bare
    // `CREATEnINDEX`, so no escape-healing heuristic is needed.
    statement
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

/// Remove SQL line comments (`-- ...` to end of line), block comments
/// (`/* ... */`), and single-quoted string contents. Stripped spans
/// are replaced by a single space so adjacent identifiers don't
/// accidentally merge.
///
/// Known gaps (rare in Django migrations, accepted for simplicity):
/// - PostgreSQL dollar-quoted strings (`$tag$ ... $tag$`) pass through
///   unchanged. Combined with statement-anchored matching (which keys off a
///   statement's *leading* tokens), an index built inside a wrapper —
///   `DO $$ ... CREATE INDEX ... $$` or a `CREATE FUNCTION` body — is
///   *missed*: the enclosing statement starts with `DO` / `CREATE FUNCTION`,
///   not `CREATE INDEX`. This is a false-*negative*. See
///   `create_index_inside_do_block_is_a_known_gap`. A `$$`-aware pass in this
///   function would close it if such migrations ever appear in practice.
/// - An unterminated single-quoted string is consumed to end-of-input —
///   false-*negative* bias for whatever followed, but real SQL with
///   unterminated quotes won't execute anyway.
/// - Nested block comments (a PostgreSQL extension) only strip up to
///   the first `*/`.
/// - Double-quoted identifiers (`"CREATE INDEX"` as an identifier name)
///   are not stripped, so a literal identifier containing the phrase
///   will false-positive.
pub(crate) fn strip_sql_noise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'-') => {
                chars.next();
                for nc in chars.by_ref() {
                    if nc == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
                // Replace with a space so tokens like `CREATE/*x*/INDEX`
                // don't accidentally merge into `CREATEINDEX` and slip
                // past the substring check.
                out.push(' ');
            }
            '\'' => {
                out.push(' ');
                while let Some(nc) = chars.next() {
                    if nc == '\'' {
                        // Doubled '' is an escaped single quote, stay in string.
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            continue;
                        }
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Strip SQL comments and string literals (via [`strip_sql_noise`]), split the
/// result into individual statements on `;`, and return whether any statement
/// satisfies `predicate`. Splitting per-statement matters: a RunSQL mixing a
/// non-concurrent and a concurrent index would otherwise be judged as a whole.
pub(crate) fn any_sql_statement(sql: &str, predicate: impl Fn(&str) -> bool) -> bool {
    strip_sql_noise(sql).split(';').any(predicate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(sql: &str) -> RunSQLOperation {
        RunSQLOperation {
            sql: sql.to_string(),
            reverse_sql: None,
        }
    }

    #[test]
    fn create_index_in_line_comment_is_ignored() {
        assert!(!op("-- CREATE INDEX foo ON t (c);\nUPDATE t SET c = 1;").contains_create_index());
    }

    #[test]
    fn create_index_in_block_comment_is_ignored() {
        assert!(!op("/* CREATE INDEX foo */ UPDATE t SET c = 1;").contains_create_index());
    }

    #[test]
    fn create_index_in_string_literal_is_ignored() {
        assert!(
            !op("INSERT INTO log (msg) VALUES ('CREATE INDEX in audit');").contains_create_index()
        );
    }

    #[test]
    fn real_create_index_still_matches() {
        assert!(op("CREATE INDEX foo ON t (c);").contains_create_index());
        assert!(op("create unique index foo ON t (c);").contains_create_index());
    }

    #[test]
    fn block_comment_between_tokens_still_matches() {
        // A block comment dropped without separator would merge tokens and
        // hide the CREATE INDEX statement from the substring check.
        assert!(op("CREATE/*x*/INDEX foo ON t (c);").contains_create_index());
    }

    #[test]
    fn drop_index_in_comment_is_ignored() {
        assert!(!op("-- DROP INDEX foo\n").contains_drop_index());
        assert!(!op("/* DROP INDEX foo */").contains_drop_index());
    }

    #[test]
    fn drop_index_real_matches() {
        assert!(op("DROP INDEX foo;").contains_drop_index());
    }

    #[test]
    fn escaped_quote_keeps_string_intact() {
        // '' inside a string is a literal apostrophe; the CREATE INDEX is
        // still inside the string and should not match.
        assert!(
            !op("INSERT INTO t (s) VALUES ('isn''t CREATE INDEX okay');").contains_create_index()
        );
    }

    #[test]
    fn create_index_inside_do_block_is_a_known_gap() {
        // Documents an accepted false-negative: `strip_sql_noise` does not
        // strip dollar-quoted bodies, and matching is anchored to a
        // statement's leading tokens, so an index created inside a `DO $$
        // ... $$` block (or a CREATE FUNCTION body) is not flagged — the
        // statement begins with `DO`, not `CREATE INDEX`. Pinned so a future
        // `$$`-aware pass that closes the gap updates this on purpose.
        assert!(!op("DO $$ BEGIN CREATE INDEX idx ON t (c); END $$;").contains_create_index());
    }

    #[test]
    fn unterminated_string_is_consumed_to_end() {
        // Pathological input: the unterminated string runs to end of input
        // so nothing past it can match. Better than misparsing and matching.
        assert!(!op("INSERT INTO t VALUES ('CREATE INDEX").contains_create_index());
    }
}

/// Data for RunPython operations.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunPythonOperation {
    /// The forward function name.
    pub code: String,
    /// The reverse function name, if provided.
    pub reverse_code: Option<String>,
}

impl RunPythonOperation {
    /// Check if this operation is reversible.
    pub fn is_reversible(&self) -> bool {
        self.reverse_code.is_some()
    }
}

/// Data for SeparateDatabaseAndState operations.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SeparateDatabaseAndStateOperation {
    /// Whether state_operations contains meaningful operations.
    ///
    /// Literal `[]` and `None` are treated as absent. Non-literal values
    /// are treated as present because rules cannot prove they are empty.
    pub has_state_operations: bool,
    /// Whether database_operations contains meaningful operations.
    ///
    /// Literal `[]` and `None` are treated as absent. Non-literal values
    /// are treated as present because rules cannot prove they are empty.
    pub has_database_operations: bool,
    /// Operations inside the `database_operations=[...]` arm.
    ///
    /// These are database-effective operations and keep their original
    /// operation spans for diagnostics and inline ignore directives.
    /// State-side operations are metadata-only and are not retained here.
    pub database_operations: Vec<Operation>,
}
