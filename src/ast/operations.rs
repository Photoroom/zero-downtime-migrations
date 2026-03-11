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

/// Type of database constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintType {
    Unique,
    Check,
    Exclusion,
    ForeignKey,
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
    /// Check if the SQL contains CREATE INDEX.
    pub fn contains_create_index(&self) -> bool {
        let sql_upper = self.sql.to_uppercase();
        sql_upper.contains("CREATE INDEX") || sql_upper.contains("CREATE UNIQUE INDEX")
    }

    /// Check if the SQL contains DROP INDEX.
    pub fn contains_drop_index(&self) -> bool {
        self.sql.to_uppercase().contains("DROP INDEX")
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
