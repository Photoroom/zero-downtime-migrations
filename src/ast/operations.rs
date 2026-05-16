//! Django migration operation types.

use crate::diagnostics::Span;

/// A Django migration operation.
#[derive(Debug, Clone)]
pub struct Operation {
    /// The type of operation.
    pub op_type: OperationType,
    /// The span of the operation in the source.
    pub span: Span,
    /// Operation-specific data.
    pub data: OperationData,
}

/// The type of a Django migration operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
pub struct IndexOperation {
    /// The model name (lowercase).
    pub model_name: String,
    /// The index name, if specified.
    pub index_name: Option<String>,
}

/// Data for model operations (CreateModel, DeleteModel, etc.).
#[derive(Debug, Clone)]
pub struct ModelOperation {
    /// The model name.
    pub name: String,
    /// Old name (for RenameModel).
    pub old_name: Option<String>,
    /// Fields (for CreateModel).
    pub fields: Vec<FieldDefinition>,
}

/// A field definition in CreateModel.
#[derive(Debug, Clone)]
pub struct FieldDefinition {
    /// The field name.
    pub name: String,
    /// The field type (e.g., "CharField", "ForeignKey").
    pub field_type: String,
    /// Whether the field is nullable.
    pub is_nullable: bool,
    /// Whether the field has a default value.
    pub has_default: bool,
    /// For ForeignKey, the referenced model.
    pub references: Option<String>,
}

/// Data for field operations (AddField, RemoveField, etc.).
#[derive(Debug, Clone)]
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
pub struct FieldInfo {
    /// The field type (e.g., "CharField", "ForeignKey").
    pub field_type: String,
    /// Whether the field is nullable.
    pub is_nullable: bool,
    /// Whether the field has a default value.
    pub has_default: bool,
    /// For ForeignKey, the referenced model.
    pub references: Option<String>,
    /// The raw field definition text.
    pub raw_text: String,
}

/// Data for constraint operations.
#[derive(Debug, Clone)]
pub struct ConstraintOperation {
    /// The model name.
    pub model_name: String,
    /// The constraint type.
    pub constraint_type: ConstraintType,
    /// The constraint name.
    pub constraint_name: Option<String>,
}

/// Type of database constraint added via `migrations.AddConstraint`.
///
/// Django ships three constraint classes (`UniqueConstraint`,
/// `CheckConstraint`, `ExclusionConstraint`); foreign keys are added via
/// `AddField`, not `AddConstraint`, so they don't appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintType {
    Unique,
    Check,
    Exclusion,
    Unknown,
}

/// Data for RunSQL operations.
#[derive(Debug, Clone)]
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
        let stripped = strip_sql_noise(&self.sql).to_uppercase();
        stripped.contains("CREATE INDEX") || stripped.contains("CREATE UNIQUE INDEX")
    }

    /// Check if the SQL contains a `DROP INDEX` statement, ignoring
    /// comments and string literals.
    pub fn contains_drop_index(&self) -> bool {
        strip_sql_noise(&self.sql)
            .to_uppercase()
            .contains("DROP INDEX")
    }
}

/// Remove SQL line comments (`-- ...` to end of line), block comments
/// (`/* ... */`), and single-quoted string contents. Stripped spans
/// are replaced by a single space so adjacent identifiers don't
/// accidentally merge.
///
/// Known gaps (rare in Django migrations, accepted for simplicity):
/// - PostgreSQL dollar-quoted strings (`$tag$ ... $tag$`) pass through
///   unchanged — false-*positive* bias if they contain CREATE/DROP
///   INDEX as plain text.
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
    fn unterminated_string_is_consumed_to_end() {
        // Pathological input: the unterminated string runs to end of input
        // so nothing past it can match. Better than misparsing and matching.
        assert!(!op("INSERT INTO t VALUES ('CREATE INDEX").contains_create_index());
    }
}

/// Data for RunPython operations.
#[derive(Debug, Clone)]
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
pub struct SeparateDatabaseAndStateOperation {
    /// Whether state_operations is present.
    pub has_state_operations: bool,
    /// Whether database_operations is present.
    pub has_database_operations: bool,
}
