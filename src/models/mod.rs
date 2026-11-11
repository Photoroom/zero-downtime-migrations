//! Conservative, non-executing ORM mapping analysis of immutable Python sources.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tree_sitter::Node;

use crate::ast::TableIdentity;
use crate::config::ModelOrm;
use crate::discovery::MigrationFramework;

pub(crate) const TRUSTED_ROOTS: &[&str] = &[
    "typing",
    "typing_extensions",
    "dataclasses",
    "re",
    "sqlalchemy",
    "functools",
    "contextlib",
    "string",
    "datetime",
    "uuid",
    "decimal",
    "contextvars",
    "pydantic",
    "tortoise",
];

const RESERVED_BINDINGS: &[&str] = &[
    "__builtins__",
    "__name__",
    "__package__",
    "__loader__",
    "__spec__",
    "__path__",
    "__getattr__",
];

pub struct ModelFile {
    pub path: PathBuf,
    pub orm: ModelOrm,
    pub source: String,
}

#[derive(Default)]
pub struct ModelIndex {
    mappings: Vec<Mapping>,
    unknown: Vec<String>,
}
#[derive(Default, Clone)]
struct TableReferences {
    targets: Vec<TableIdentity>,
    unknown: Vec<String>,
}
struct Mapping {
    orm: ModelOrm,
    table: TableIdentity,
    columns: HashMap<String, String>,
    unknown: Vec<String>,
    references: TableReferences,
}
#[derive(Clone, Debug)]
enum Expr {
    Name(String),
    String(String),
    Bool(bool),
    Call(Box<Expr>, Vec<Expr>, HashMap<String, Expr>),
    Subscript(Box<Expr>, Vec<Expr>),
    Dict(HashMap<String, Expr>),
    Mapping(Vec<(Expr, Expr)>),
    Literal,
    None,
    Limit,
    Tuple(Vec<Expr>),
    Union(Vec<Expr>),
    Add(Vec<Expr>),
    Other,
    Missing,
    DeferredDefault,
}
#[derive(Clone)]
struct Attribute {
    name: String,
    value: Expr,
    annotation: Expr,
    location: String,
}
#[derive(Clone)]
struct Class {
    name: String,
    module: String,
    orm: ModelOrm,
    bases: Vec<Expr>,
    attrs: Vec<Attribute>,
    meta: Vec<Attribute>,
    unsupported: bool,
    decorators: Vec<Expr>,
    has_methods: bool,
    data_body: bool,
    audited: bool,
    location: String,
}
#[derive(Default)]
struct Sources {
    classes: HashMap<String, Class>,
    aliases: HashMap<String, (Expr, String)>,
    unknown: Vec<String>,
    proof_work: Cell<usize>,
    inert_records: RefCell<HashSet<(String, usize)>>,
    resolved_classes: RefCell<HashMap<(String, bool, usize), Resolved>>,
    symbol_cache: RefCell<HashMap<(String, String, usize), String>>,
    cache_symbols: bool,
    mutated: HashSet<String>,
    mutation_aliases: Vec<(String, String)>,
    mutation_calls: HashSet<String>,
    ambiguous: HashSet<String>,
    imported_modules: HashMap<String, (PathBuf, ModelOrm)>,
}
#[derive(Default, Clone)]
struct Resolved {
    model: bool,
    has_relationships: bool,
    table: Option<String>,
    schema: Option<String>,
    columns: HashMap<String, String>,
    mapped_attributes: HashSet<String>,
    unknown: Vec<String>,
    references: TableReferences,
    abstract_model: bool,
    primary_key: bool,
    identity_unknown: bool,
    eager_unknown: bool,
}

impl ModelIndex {
    pub fn build(files: Vec<ModelFile>) -> Self {
        Self::build_with_dependencies(files, |_| Ok(None))
    }

    pub(crate) fn build_with_dependencies(
        files: Vec<ModelFile>,
        mut dependency: impl FnMut(&str) -> Result<Option<ModelFile>, String>,
    ) -> Self {
        let mut sources = Sources {
            proof_work: Cell::new(4096),
            ..Sources::default()
        };
        for file in files {
            if TRUSTED_ROOTS.contains(&module(&file.path).split('.').next().unwrap_or("")) {
                sources.unknown.push(format!(
                    "{}: repository shadows a supported library",
                    file.path.display()
                ));
            }
            sources.read(file, None);
        }
        let mut requested: HashSet<String> = HashSet::new();
        let mut discovered = HashSet::new();
        let mut discovered_aliases = HashSet::new();
        loop {
            let mut names = Vec::new();
            for (key, class) in &sources.classes {
                if (!class.audited && !requested.contains(key)) || !discovered.insert(key.clone()) {
                    continue;
                }
                for base in &class.bases {
                    sources.constructor_names(base, &class.module, &mut names);
                }
                if !class.audited
                    && !class.bases.iter().any(|base| {
                        sources.symbol(base, &class.module, 0).as_deref()
                            == Some("typing.NamedTuple")
                    })
                {
                    continue;
                }
                for attr in &class.attrs {
                    sources.constructor_names(&attr.value, &class.module, &mut names);
                    sources.constructor_names(&attr.annotation, &class.module, &mut names);
                }
            }
            // Follow selected alias expressions without expanding unrelated module values.
            for name in &requested {
                if let Some((value, origin)) = sources.aliases.get(name) {
                    if discovered_aliases.insert(name.clone()) {
                        sources.constructor_names(value, origin, &mut names);
                    }
                }
            }
            let mut progress = false;
            for name in names {
                if !name.contains('.') || !requested.insert(name.clone()) {
                    continue;
                }
                if requested.len() > 4096 {
                    sources
                        .unknown
                        .push("dependency resolution work limit exceeded".into());
                    break;
                }
                if !sources.proof_step() {
                    sources
                        .unknown
                        .push("dependency resolution work limit exceeded".into());
                    break;
                }
                progress = true;
                match dependency(&name) {
                    Ok(Some(file)) => {
                        sources.read(file, Some(&name));
                        progress = true;
                    }
                    Ok(None) => {}
                    Err(reason) => sources.unknown.push(reason),
                }
            }
            if !progress {
                break;
            }
        }
        for name in &sources.ambiguous {
            if requested.contains(name)
                || sources.classes.get(name).is_some_and(|class| class.audited)
            {
                sources.unknown.push(format!("symbol {name} rebound"));
            }
        }
        sources.cache_symbols = true;
        let mut mutations: Vec<_> = std::mem::take(&mut sources.mutated)
            .into_iter()
            .map(|name| (name, false, 0))
            .chain(
                sources
                    .mutation_calls
                    .iter()
                    .cloned()
                    .map(|name| (name, true, 0)),
            )
            .collect();
        let mut seen_mutations = HashSet::new();
        while let Some((name, call, depth)) = mutations.pop() {
            if !seen_mutations.insert((name.clone(), call)) {
                continue;
            }
            if depth > 32 || !sources.proof_step() {
                sources
                    .unknown
                    .push("mutation binding resolution work limit exceeded".into());
                break;
            }
            if let Some(target) = sources.symbol(&Expr::Name(name.clone()), "", 0) {
                if call {
                    if matches!(
                        target.as_str(),
                        "setattr"
                            | "delattr"
                            | "exec"
                            | "eval"
                            | "builtins.setattr"
                            | "builtins.delattr"
                            | "builtins.exec"
                            | "builtins.eval"
                    ) {
                        sources
                            .unknown
                            .push(format!("{name}: explicit namespace mutation"));
                    }
                } else {
                    let mut prefix = target.as_str();
                    loop {
                        if sources.classes.contains_key(prefix) {
                            sources.mutated.insert(prefix.to_owned());
                            break;
                        }
                        let Some((parent, _)) = prefix.rsplit_once('.') else {
                            break;
                        };
                        prefix = parent;
                    }
                }
            }
            for (alias, target) in &sources.mutation_aliases {
                if name == *alias {
                    mutations.push((target.clone(), call, depth + 1));
                } else if let Some(suffix) = name
                    .strip_prefix(alias)
                    .filter(|suffix| suffix.starts_with('.'))
                {
                    mutations.push((format!("{target}{suffix}"), call, depth + 1));
                }
            }
        }
        let mut index = Self {
            mappings: vec![],
            unknown: sources.unknown.clone(),
        };
        for class in sources.classes.values().filter(|class| class.audited) {
            let resolved = sources.resolve_class(class, &mut HashSet::new(), true);
            if resolved.eager_unknown {
                index.unknown.push(format!(
                    "{}: eager class expression is unresolved ({})",
                    class.location,
                    describe(&resolved.unknown)
                ));
            }
            if !resolved.model || resolved.abstract_model {
                continue;
            }
            if resolved.table.as_deref().is_some_and(str::is_empty)
                || resolved.schema.as_deref().is_some_and(str::is_empty)
            {
                index
                    .unknown
                    .push(format!("{}: empty table/schema identity", class.location));
            }
            if resolved.identity_unknown {
                index.unknown.push(format!(
                    "{}: model table identity is unresolved ({})",
                    class.location,
                    describe(&resolved.unknown)
                ));
            }
            if let Some(table) = resolved.table {
                index.mappings.push(Mapping {
                    orm: class.orm,
                    table: TableIdentity {
                        name: table,
                        schema: resolved.schema,
                    },
                    columns: resolved.columns,
                    unknown: resolved.unknown,
                    references: resolved.references,
                });
            } else if !resolved.abstract_model || !resolved.unknown.is_empty() {
                index.unknown.push(format!(
                    "{}: model table cannot be resolved ({})",
                    class.location,
                    describe(&resolved.unknown)
                ));
            }
        }
        if sources.proof_work.get() == 0 {
            index
                .unknown
                .push("shared model resolution/expansion work limit exceeded".into());
        }
        index
    }

    fn complete_tables(&self) -> Result<(), String> {
        let reasons: Vec<_> = self
            .unknown
            .iter()
            .chain(self.mappings.iter().flat_map(|m| m.unknown.iter()))
            .cloned()
            .collect();
        if !reasons.is_empty() {
            return Err(format!(
                "model coverage is unresolved: {}",
                describe(&reasons)
            ));
        }
        Ok(())
    }

    pub fn table_presence(
        &self,
        framework: MigrationFramework,
        table: &TableIdentity,
    ) -> Result<(), String> {
        let required = match framework {
            MigrationFramework::Alembic => ModelOrm::Sqlalchemy,
            MigrationFramework::Aerich => ModelOrm::Tortoise,
            _ => return Err("ORM model evidence is unavailable for this framework".into()),
        };
        self.complete_tables()?;
        if !self
            .mappings
            .iter()
            .any(|m| m.orm == required && m.table == *table)
        {
            return Err(format!(
                "no complete {:?} mapping for table {}",
                required, table.name
            ));
        }
        Ok(())
    }

    pub fn table_absence(&self, table: &TableIdentity) -> Result<(), String> {
        self.complete_tables()?;
        for mapping in &self.mappings {
            if tables_overlap(&mapping.table, table) {
                return Err(format!("table {} is still mapped", table.name));
            }
            if !mapping.references.unknown.is_empty() {
                return Err(format!(
                    "table references are unresolved: {}",
                    describe(&mapping.references.unknown)
                ));
            }
            if mapping
                .references
                .targets
                .iter()
                .any(|target| tables_overlap(target, table))
            {
                return Err(format!(
                    "table {} is still referenced by {}",
                    table.name, mapping.table.name
                ));
            }
        }
        Ok(())
    }

    pub fn column_absence(
        &self,
        framework: MigrationFramework,
        table: &TableIdentity,
        column: &str,
    ) -> Result<(), String> {
        let required = match framework {
            MigrationFramework::Alembic => ModelOrm::Sqlalchemy,
            MigrationFramework::Aerich => ModelOrm::Tortoise,
            _ => return Err("ORM model evidence is unavailable for this framework".into()),
        };
        let matching: Vec<_> = self
            .mappings
            .iter()
            .filter(|m| {
                m.table.name == table.name
                    && (m.table.schema == table.schema
                        || m.table.schema.is_none()
                        || table.schema.is_none())
            })
            .collect();
        // Preserve definite usage even where other mappings remain unknown.
        for mapping in &matching {
            if let Some(location) = mapping.columns.get(column) {
                return Err(format!("column {column} is still mapped at {location}"));
            }
        }
        if !self.unknown.is_empty() {
            return Err(format!(
                "model coverage is unresolved: {}",
                describe(&self.unknown)
            ));
        }
        if !matching
            .iter()
            .any(|m| m.orm == required && m.table == *table)
        {
            return Err(format!(
                "no complete {:?} mapping for table {}",
                required, table.name
            ));
        }
        for mapping in matching {
            if !mapping.unknown.is_empty() {
                return Err(format!(
                    "table {} mapping is unresolved: {}",
                    table.name,
                    describe(&mapping.unknown)
                ));
            }
        }
        Ok(())
    }
}

fn tables_overlap(left: &TableIdentity, right: &TableIdentity) -> bool {
    left.name == right.name
        && (left.schema == right.schema || left.schema.is_none() || right.schema.is_none())
}

const TORTOISE_FIELDS: &[&str] = &[
    "IntField",
    "SmallIntField",
    "BigIntField",
    "CharField",
    "TextField",
    "BooleanField",
    "DecimalField",
    "FloatField",
    "DatetimeField",
    "DateField",
    "TimeDeltaField",
    "TimeField",
    "JSONField",
    "UUIDField",
    "BinaryField",
    "ForeignKeyField",
    "OneToOneField",
    "IntEnumField",
    "CharEnumField",
];

fn sql_type(name: &str) -> bool {
    name == "sqlalchemy.dialects.postgresql.JSONB"
        || [
            "sqlalchemy.",
            "sqlalchemy.types.",
            "sqlalchemy.sql.sqltypes.",
        ]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix).is_some_and(|name| {
                matches!(
                    name,
                    "BigInteger"
                        | "Integer"
                        | "SmallInteger"
                        | "String"
                        | "Text"
                        | "Boolean"
                        | "DateTime"
                        | "Date"
                        | "Time"
                        | "LargeBinary"
                        | "UUID"
                        | "Float"
                        | "Numeric"
                        | "JSON"
                        | "Enum"
                )
            })
        })
}

fn registry_key(value: &Expr) -> bool {
    match value {
        Expr::Name(name) => matches!(
            name.as_str(),
            "str"
                | "int"
                | "bool"
                | "float"
                | "bytes"
                | "dict"
                | "list"
                | "tuple"
                | "datetime.datetime"
        ),
        Expr::Subscript(inner, args) => {
            matches!(&**inner, Expr::Name(name) if matches!(name.as_str(), "dict" | "list" | "tuple"))
                && !args.is_empty()
                && args.iter().all(registry_key)
        }
        _ => false,
    }
}

fn safe_schema_call(name: &str) -> bool {
    name.strip_prefix("tortoise.fields.")
        .is_some_and(|field| TORTOISE_FIELDS.contains(&field))
        || matches!(
            name,
            "sqlalchemy.orm.mapped_column"
                | "sqlalchemy.func.now"
                | "sqlalchemy.dialects.postgresql.JSONB"
                | "dict"
                | "list"
                | "tuple"
                | "set"
                | "frozenset"
        )
        || [
            "sqlalchemy.",
            "sqlalchemy.schema.",
            "sqlalchemy.sql.schema.",
            "sqlalchemy.types.",
            "sqlalchemy.sql.sqltypes.",
        ]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix).is_some_and(|name| {
                matches!(
                    name,
                    "Column"
                        | "Index"
                        | "UniqueConstraint"
                        | "CheckConstraint"
                        | "PrimaryKeyConstraint"
                        | "Identity"
                        | "Sequence"
                        | "Computed"
                        | "text"
                        | "BigInteger"
                        | "Integer"
                        | "SmallInteger"
                        | "String"
                        | "Text"
                        | "Boolean"
                        | "DateTime"
                        | "Date"
                        | "Time"
                        | "LargeBinary"
                        | "UUID"
                        | "Float"
                        | "Numeric"
                        | "JSON"
                        | "Enum"
                )
            })
        })
}

impl TableReferences {
    fn foreign_key(&mut self, value: Option<&Expr>) {
        if let Some(Expr::String(target)) = value {
            let parts: Vec<_> = target.split('.').collect();
            if (2..=3).contains(&parts.len())
                && parts.iter().all(|part| {
                    !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
            {
                self.targets.push(TableIdentity {
                    name: parts[parts.len() - 2].into(),
                    schema: (parts.len() == 3).then(|| parts[0].into()),
                });
                return;
            }
        }
        self.unknown.push("unsupported foreign-key target".into());
    }

    /// Inspect expanded expressions separately: column retirement does not need target resolution.
    fn collect(&mut self, value: &Expr) {
        match value {
            Expr::Call(function, args, kwargs) => {
                if let Expr::Name(name) = &**function {
                    match name.as_str() {
                        "sqlalchemy.ForeignKey"
                        | "sqlalchemy.schema.ForeignKey"
                        | "sqlalchemy.sql.schema.ForeignKey" => {
                            self.foreign_key(args.first().or_else(|| kwargs.get("column")));
                            if kwargs.contains_key("**") {
                                self.unknown.push("dynamic foreign-key options".into());
                            }
                        }
                        "sqlalchemy.ForeignKeyConstraint"
                        | "sqlalchemy.schema.ForeignKeyConstraint" => {
                            match args.get(1).or_else(|| kwargs.get("refcolumns")) {
                                Some(Expr::Tuple(targets)) if !targets.is_empty() => {
                                    for target in targets {
                                        self.foreign_key(Some(target));
                                    }
                                }
                                _ => self
                                    .unknown
                                    .push("unsupported foreign-key constraint targets".into()),
                            }
                            if kwargs.contains_key("**") {
                                self.unknown.push("dynamic foreign-key options".into());
                            }
                        }
                        "sqlalchemy.orm.relationship" => self
                            .unknown
                            .push("relationship target is unresolved".into()),
                        _ if name.starts_with("tortoise.fields.")
                            && matches!(
                                name.rsplit('.').next(),
                                Some(
                                    "ForeignKeyField"
                                        | "OneToOneField"
                                        | "ManyToManyField"
                                        | "BackwardFKRelation"
                                        | "BackwardOneToOneRelation"
                                )
                            ) =>
                        {
                            self.unknown
                                .push("Tortoise relationship target is unresolved".into())
                        }
                        _ if safe_schema_call(name) => {
                            if matches!(
                                name.as_str(),
                                "sqlalchemy.orm.mapped_column"
                                    | "sqlalchemy.Column"
                                    | "sqlalchemy.schema.Column"
                                    | "sqlalchemy.sql.schema.Column"
                            ) {
                                for arg in args {
                                    if !matches!(arg, Expr::String(_) | Expr::Call(_, _, _))
                                        && !matches!(arg, Expr::Name(name) if safe_schema_call(name))
                                    {
                                        self.unknown
                                            .push("unresolved column schema argument".into());
                                    }
                                }
                            }
                        }
                        _ => self
                            .unknown
                            .push(format!("unsupported reference expression {name}")),
                    }
                } else {
                    self.unknown.push("unresolved reference expression".into());
                }
                self.collect(function);
                for value in args {
                    self.collect(value);
                }
                for (key, value) in kwargs {
                    if key == "default"
                        && matches!(value, Expr::DeferredDefault)
                        && matches!(&**function, Expr::Name(name) if matches!(name.as_str(), "sqlalchemy.orm.mapped_column" | "sqlalchemy.Column" | "sqlalchemy.schema.Column" | "sqlalchemy.sql.schema.Column"))
                    {
                        continue;
                    }
                    self.collect(value);
                }
            }
            Expr::Subscript(inner, args) => {
                self.collect(inner);
                for value in args {
                    self.collect(value);
                }
            }
            Expr::Dict(values) => {
                for value in values.values() {
                    self.collect(value);
                }
            }
            Expr::Mapping(values) => {
                for (key, value) in values {
                    self.collect(key);
                    self.collect(value);
                }
            }
            Expr::Tuple(values) | Expr::Union(values) => {
                for value in values {
                    self.collect(value);
                }
            }
            Expr::Other | Expr::DeferredDefault | Expr::Add(_) => self
                .unknown
                .push("unsupported eager reference expression".into()),
            Expr::Limit => self
                .unknown
                .push("reference expansion limit exceeded".into()),
            _ => {}
        }
    }
}

// SQLAlchemy unwraps a single type plus None before inspecting Annotated metadata.
fn non_nullable_type(value: &Expr) -> &Expr {
    let args = match value {
        Expr::Union(args) => args,
        Expr::Subscript(inner, args)
            if matches!(&**inner, Expr::Name(name) if name == "typing.Optional")
                && args.len() == 1 =>
        {
            return non_nullable_type(&args[0])
        }
        Expr::Subscript(inner, args) if matches!(&**inner, Expr::Name(name) if name == "typing.Union") => {
            args
        }
        _ => return value,
    };
    let mut types = args.iter().filter(|arg| !matches!(arg, Expr::None));
    match (types.next(), types.next()) {
        (Some(single), None) => non_nullable_type(single),
        _ => value,
    }
}

fn unsafe_forward_annotation(value: &Expr) -> bool {
    match value {
        Expr::String(name) => name.split('.').any(|part| {
            let mut chars = part.chars();
            !chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }),
        Expr::Subscript(inner, args) => {
            unsafe_forward_annotation(inner) || args.iter().any(unsafe_forward_annotation)
        }
        Expr::Tuple(args) | Expr::Union(args) => args.iter().any(unsafe_forward_annotation),
        Expr::Literal => true,
        _ => false,
    }
}

fn contains_limit(value: &Expr) -> bool {
    match value {
        Expr::Limit => true,
        Expr::Call(function, args, kwargs) => {
            contains_limit(function)
                || args.iter().any(contains_limit)
                || kwargs.values().any(contains_limit)
        }
        Expr::Subscript(inner, args) => contains_limit(inner) || args.iter().any(contains_limit),
        Expr::Dict(values) => values.values().any(contains_limit),
        Expr::Mapping(values) => values
            .iter()
            .any(|(k, v)| contains_limit(k) || contains_limit(v)),
        Expr::Tuple(values) | Expr::Union(values) | Expr::Add(values) => {
            values.iter().any(contains_limit)
        }
        _ => false,
    }
}

fn describe(reasons: &[String]) -> String {
    let mut reasons: Vec<_> = reasons.iter().collect();
    reasons.sort();
    reasons.dedup();
    let mut result = reasons
        .iter()
        .take(3)
        .map(|reason| {
            let truncated: String = reason.chars().take(400).collect();
            if reason.chars().count() > 400 {
                format!("{truncated}…")
            } else {
                truncated
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    if reasons.len() > 3 {
        result.push_str(&format!(
            "; and {} more unresolved items",
            reasons.len() - 3
        ));
    }
    result
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}
fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect()
}
fn expr(node: Node<'_>, source: &str) -> Expr {
    match node.kind() {
        "identifier" => Expr::Name(text(node, source).to_owned()),
        "attribute" => {
            let mut parts = Vec::new();
            let mut current = node;
            loop {
                match current.kind() {
                    "attribute" => {
                        let (Some(object), Some(attribute)) = (
                            current.child_by_field_name("object"),
                            current.child_by_field_name("attribute"),
                        ) else {
                            return Expr::Other;
                        };
                        parts.push(text(attribute, source));
                        current = object;
                    }
                    "parenthesized_expression" => {
                        let children = named(current);
                        if children.len() != 1 {
                            return Expr::Other;
                        }
                        current = children[0];
                    }
                    "identifier" => {
                        parts.push(text(current, source));
                        break;
                    }
                    _ => return Expr::Other,
                }
            }
            parts.reverse();
            Expr::Name(parts.join("."))
        }
        "string" => {
            let raw = text(node, source);
            let raw = raw
                .strip_prefix('r')
                .or_else(|| raw.strip_prefix('R'))
                .unwrap_or(raw);
            if raw.len() >= 2
                && (raw.starts_with('\'') || raw.starts_with('"'))
                && !raw.contains('\\')
                && !raw.starts_with("\"\"\"")
                && !raw.starts_with("'''")
            {
                Expr::String(raw[1..raw.len() - 1].into())
            } else if literal_docstring(node) {
                Expr::Literal
            } else {
                Expr::Other
            }
        }
        "concatenated_string" if named(node).iter().all(|node| literal_docstring(*node)) => {
            Expr::Literal
        }
        "lambda"
            if node.child_by_field_name("parameters").is_none()
                && node.parent().is_some_and(|parent| {
                    parent.kind() == "keyword_argument"
                        && parent
                            .child_by_field_name("name")
                            .is_some_and(|name| text(name, source) == "default")
                }) =>
        {
            Expr::DeferredDefault
        }
        "integer" | "float" => Expr::Literal,
        "none" => Expr::None,
        "tuple" | "list" => Expr::Tuple(named(node).iter().map(|n| expr(*n, source)).collect()),
        "true" => Expr::Bool(true),
        "false" => Expr::Bool(false),
        "call" => {
            let Some(function) = node.child_by_field_name("function") else {
                return Expr::Other;
            };
            let mut positional = vec![];
            let mut keywords = HashMap::new();
            if let Some(args) = node.child_by_field_name("arguments") {
                for arg in named(args) {
                    if arg.kind() == "keyword_argument" {
                        if let (Some(name), Some(value)) = (
                            arg.child_by_field_name("name"),
                            arg.child_by_field_name("value"),
                        ) {
                            keywords.insert(text(name, source).into(), expr(value, source));
                        }
                    } else if arg.kind() == "dictionary_splat" || arg.kind() == "list_splat" {
                        keywords.insert("**".into(), Expr::Other);
                    } else {
                        positional.push(expr(arg, source));
                    }
                }
            }
            Expr::Call(Box::new(expr(function, source)), positional, keywords)
        }
        "binary_operator"
            if node
                .child_by_field_name("operator")
                .is_some_and(|n| text(n, source) == "|") =>
        {
            Expr::Union(named(node).iter().map(|n| expr(*n, source)).collect())
        }
        "binary_operator"
            if node
                .child_by_field_name("operator")
                .is_some_and(|n| text(n, source) == "+") =>
        {
            Expr::Add(named(node).iter().map(|n| expr(*n, source)).collect())
        }
        "union_type" => Expr::Union(named(node).iter().map(|n| expr(*n, source)).collect()),
        "generic_type" => {
            let children = named(node);
            if children.len() != 2 {
                return Expr::Other;
            }
            Expr::Subscript(
                Box::new(expr(children[0], source)),
                named(children[1])
                    .iter()
                    .map(|n| expr(*n, source))
                    .collect(),
            )
        }
        "subscript" => {
            let children = named(node);
            if children.is_empty() {
                return Expr::Other;
            }
            Expr::Subscript(
                Box::new(expr(children[0], source)),
                children[1..].iter().map(|n| expr(*n, source)).collect(),
            )
        }
        "type" => named(node)
            .first()
            .map_or(Expr::Other, |n| expr(*n, source)),
        "dictionary" => {
            let mut pairs = vec![];
            for pair in named(node) {
                let (Some(key), Some(value)) = (
                    pair.child_by_field_name("key"),
                    pair.child_by_field_name("value"),
                ) else {
                    return Expr::Other;
                };
                pairs.push((expr(key, source), expr(value, source)));
            }
            if pairs.iter().all(|(key, _)| matches!(key, Expr::String(_))) {
                let mut keys = HashSet::new();
                if pairs
                    .iter()
                    .any(|(key, _)| matches!(key, Expr::String(key) if !keys.insert(key)))
                {
                    return Expr::Other;
                }
                Expr::Dict(
                    pairs
                        .into_iter()
                        .filter_map(|(key, value)| {
                            if let Expr::String(key) = key {
                                Some((key, value))
                            } else {
                                None
                            }
                        })
                        .collect(),
                )
            } else {
                Expr::Mapping(pairs)
            }
        }
        _ => Expr::Other,
    }
}
fn module(path: &std::path::Path) -> String {
    let name = path
        .with_extension("")
        .to_string_lossy()
        .replace(['/', '\\'], ".");
    name.strip_suffix(".__init__").unwrap_or(&name).to_owned()
}
fn literal_docstring(node: Node<'_>) -> bool {
    node.kind() == "string" && named(node).iter().all(|n| n.kind() != "interpolation")
}

fn attrs(body: Node<'_>, source: &str, path: &std::path::Path) -> (Vec<Attribute>, bool) {
    let mut result = vec![];
    let mut unsupported = false;
    for statement in named(body) {
        if statement.kind() == "comment" || statement.kind() == "pass_statement" {
            continue;
        }
        if statement.kind() == "expression_statement" {
            let children = named(statement);
            if children.len() != 1 {
                unsupported = true;
                continue;
            }
            if children.first().is_some_and(|n| literal_docstring(*n)) {
                continue;
            }
            if let Some(assignment) = children.first().filter(|n| n.kind() == "assignment") {
                if let Some(left) = assignment.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        result.push(Attribute {
                            name: text(left, source).into(),
                            value: assignment
                                .child_by_field_name("right")
                                .map_or(Expr::Missing, |n| expr(n, source)),
                            annotation: assignment
                                .child_by_field_name("type")
                                .map_or(Expr::Missing, |n| expr(n, source)),
                            location: format!(
                                "{}:{} ({})",
                                path.display(),
                                left.start_position().row + 1,
                                text(left, source)
                            ),
                        });
                        continue;
                    }
                }
            }
        }
        // Class construction hooks can mutate mappings even without decorators.
        if statement.kind() == "function_definition"
            && statement.child_by_field_name("name").is_some_and(|n| {
                matches!(
                    text(n, source),
                    "__init_subclass__"
                        | "__table_cls__"
                        | "__mapper_cls__"
                        | "__new__"
                        | "__class_getitem__"
                        | "__declare_first__"
                        | "__declare_last__"
                        | "__sa_decl_prepare__"
                        | "__sa_decl_prepare_nocascade__"
                        | "__set_name__"
                )
            })
        {
            unsupported = true;
        }
        // Ordinary instance methods are outside model-declaration analysis.
        if statement.kind() != "function_definition" && statement.kind() != "class_definition" {
            unsupported = true;
        }
    }
    (result, unsupported)
}

fn binding_target(node: Node<'_>, source: &str, name: &str) -> bool {
    match node.kind() {
        "identifier" => text(node, source) == name,
        "attribute" => node
            .child_by_field_name("object")
            .is_some_and(|n| binding_target(n, source, name)),
        "subscript" => node
            .child_by_field_name("value")
            .is_some_and(|n| binding_target(n, source, name)),
        _ => named(node)
            .into_iter()
            .any(|n| binding_target(n, source, name)),
    }
}

fn assignment_names(node: Node<'_>, source: &str, attributes_only: bool, names: &mut Vec<String>) {
    match node.kind() {
        "attribute" => {
            if let Expr::Name(name) = expr(node, source) {
                names.push(name.split('.').next().unwrap_or("").to_owned());
            }
        }
        "identifier" if !attributes_only => names.push(text(node, source).to_owned()),
        "subscript" => {
            if let Some(value) = node.child_by_field_name("value") {
                assignment_names(value, source, attributes_only, names);
            }
        }
        "pattern_list"
        | "tuple_pattern"
        | "list_pattern"
        | "tuple"
        | "list"
        | "parenthesized_expression" => {
            for child in named(node) {
                assignment_names(child, source, attributes_only, names);
            }
        }
        _ => {}
    }
}

fn alias_targets(value: &Expr, names: &mut Vec<String>) {
    match value {
        Expr::Name(name) => names.push(name.clone()),
        Expr::Tuple(values) => {
            for value in values {
                alias_targets(value, names);
            }
        }
        _ => {}
    }
}

fn mentions_binding(node: Node<'_>, source: &str, name: &str) -> bool {
    if node.kind() == "function_definition" {
        return node
            .child_by_field_name("name")
            .is_some_and(|n| text(n, source) == name);
    }
    if matches!(
        node.kind(),
        "assignment" | "augmented_assignment" | "named_expression"
    ) {
        return node
            .child_by_field_name("left")
            .or_else(|| node.child_by_field_name("name"))
            .is_some_and(|n| binding_target(n, source, name));
    }
    if node.kind() == "delete_statement" {
        return named(node).iter().any(|n| binding_target(*n, source, name));
    }
    if matches!(node.kind(), "import_from_statement" | "import_statement") {
        let mut context = Sources::default();
        context.import(node, source, "context", false);
        return context.aliases.contains_key(&format!("context.{name}"));
    }
    (node.kind() == "class_definition"
        && node
            .child_by_field_name("name")
            .is_some_and(|n| text(n, source) == name))
        || named(node)
            .into_iter()
            .any(|n| mentions_binding(n, source, name))
}

fn namespace_mutation(node: Node<'_>, source: &str, binding: Option<&str>) -> bool {
    if let (Some(required), Some(left)) = (binding, node.child_by_field_name("left")) {
        if let Expr::Subscript(owner, keys) = expr(left, source) {
            if matches!(&*owner, Expr::Call(f, args, kwargs) if matches!(&**f, Expr::Name(name) if matches!(name.as_str(), "globals" | "locals")) && args.is_empty() && kwargs.is_empty())
                && matches!(keys.as_slice(), [Expr::String(key)] if key != required && !RESERVED_BINDINGS.contains(&key.as_str()))
            {
                return node
                    .child_by_field_name("right")
                    .is_some_and(|right| namespace_mutation(right, source, binding));
            }
        }
    }
    (node.kind() == "call"
        && node
            .child_by_field_name("function")
            .is_some_and(|n| matches!(text(n, source), "exec" | "eval" | "setattr" | "delattr")))
        || (matches!(
            node.kind(),
            "assignment" | "augmented_assignment" | "named_expression" | "delete_statement"
        ) && named(node).first().is_some_and(|n| {
            let target = text(*n, source);
            binding_target(*n, source, "globals")
                || binding_target(*n, source, "locals")
                || RESERVED_BINDINGS
                    .iter()
                    .any(|name| binding_target(*n, source, name))
                || [
                    ".modules",
                    ".meta_path",
                    ".path_hooks",
                    ".__dict__",
                    ".__path__",
                    ".__loader__",
                    ".__spec__",
                    ".__package__",
                ]
                .iter()
                .any(|part| target.contains(part))
        }))
        || named(node)
            .into_iter()
            .any(|n| namespace_mutation(n, source, binding))
}

fn data_body(body: Node<'_>) -> bool {
    named(body).into_iter().all(|node| match node.kind() {
        "function_definition" | "pass_statement" => true,
        "decorated_definition" => node
            .child_by_field_name("definition")
            .is_some_and(|n| n.kind() == "function_definition"),
        "expression_statement" => {
            named(node).len() == 1
                && named(node).first().is_some_and(|child| {
                    literal_docstring(*child)
                        || (child.kind() == "assignment"
                            && child
                                .child_by_field_name("left")
                                .is_some_and(|left| left.kind() == "identifier"))
                })
        }
        _ => false,
    })
}

/// Package context cannot silently redirect the exact imported submodule.
pub(crate) fn check_package(
    path: &std::path::Path,
    parsed: &crate::parser::ParsedMigration,
    child: &str,
    _required: &str,
) -> Result<(), String> {
    if parsed.has_errors() {
        return Err(format!("{}: invalid package context", path.display()));
    }
    for node in named(parsed.root_node()) {
        if matches!(node.kind(), "import_from_statement" | "import_statement") {
            let mut context = Sources::default();
            let module = module(path);
            context.import(node, &parsed.source, &module, true);
            if !context.unknown.is_empty() {
                return Err(format!("{}: ambiguous package import", path.display()));
            }
            let child_module = format!("{module}.{child}");
            if let Some((target, origin)) = context.aliases.get(&child_module) {
                if !matches!(target, Expr::Name(name) if origin.is_empty() && name == &child_module)
                {
                    return Err(format!(
                        "{}: package binding {child} is redirected",
                        path.display()
                    ));
                }
            }
            continue;
        }
        if mentions_binding(node, &parsed.source, child)
            || namespace_mutation(node, &parsed.source, Some(child))
        {
            return Err(format!(
                "{}: package binding {child} is unresolved",
                path.display()
            ));
        }
    }
    Ok(())
}

impl Resolved {
    fn copy_cost(&self) -> usize {
        1 + usize::from(self.table.is_some())
            + usize::from(self.schema.is_some())
            + 2 * self.columns.len()
            + self.mapped_attributes.len()
            + self
                .references
                .targets
                .iter()
                .map(|target| 1 + usize::from(target.schema.is_some()))
                .sum::<usize>()
    }
}

impl Sources {
    fn registry(&self, value: &Expr, module: &str) -> bool {
        let Expr::Call(function, args, kwargs) = self.expand(value, module, 0) else {
            return false;
        };
        self.symbol(&function, module, 0).as_deref() == Some("sqlalchemy.orm.registry")
            && args.is_empty()
            && kwargs.keys().all(|key| key == "type_annotation_map")
            && kwargs.get("type_annotation_map").is_none_or(|map| {
                matches!(map, Expr::Mapping(values)
                if values.iter().all(|(key, value)| registry_key(key) && self.schema_type(value)))
            })
    }

    fn schema_type(&self, value: &Expr) -> bool {
        match value {
            Expr::Name(name) => sql_type(name),
            Expr::Call(function, args, kwargs) => {
                matches!(&**function, Expr::Name(name) if sql_type(name))
                    && !kwargs.contains_key("**")
                    && args.iter().chain(kwargs.values()).all(|v| {
                        self.static_scalar(v, "") || matches!(v, Expr::Bool(_) | Expr::None)
                    })
            }
            _ => false,
        }
    }

    fn eager_effects(&self, value: &Expr) -> bool {
        let value = self.reference_value(value);
        let mut effects = TableReferences::default();
        if let Expr::Call(function, args, kwargs) = &value {
            if matches!(&**function, Expr::Name(name) if matches!(name.as_str(),
                "sqlalchemy.orm.relationship" | "sqlalchemy.orm.column_property" |
                "sqlalchemy.orm.synonym" | "sqlalchemy.orm.registry"))
            {
                for value in args.iter().chain(kwargs.values()) {
                    effects.collect(value);
                }
            } else {
                effects.collect(&value);
            }
        } else {
            effects.collect(&value);
        }
        effects.unknown.iter().any(|reason| {
            reason.starts_with("unsupported reference expression")
                || matches!(
                    reason.as_str(),
                    "unresolved reference expression"
                        | "unsupported eager reference expression"
                        | "reference expansion limit exceeded"
                        | "unresolved column schema argument"
                )
        })
    }

    fn reference_value(&self, value: &Expr) -> Expr {
        if (matches!(value, Expr::Add(_))
            || matches!(value, Expr::Call(function, _, _) if matches!(&**function, Expr::Name(name) if name == "len")))
            && self.static_scalar(value, "")
        {
            return Expr::Literal;
        }
        match value {
            Expr::Call(function, args, kwargs) => Expr::Call(
                function.clone(),
                args.iter().map(|v| self.reference_value(v)).collect(),
                kwargs
                    .iter()
                    .map(|(k, v)| (k.clone(), self.reference_value(v)))
                    .collect(),
            ),
            Expr::Subscript(inner, args) => Expr::Subscript(
                Box::new(self.reference_value(inner)),
                args.iter().map(|v| self.reference_value(v)).collect(),
            ),
            Expr::Tuple(values) => {
                Expr::Tuple(values.iter().map(|v| self.reference_value(v)).collect())
            }
            Expr::Union(values) => {
                Expr::Union(values.iter().map(|v| self.reference_value(v)).collect())
            }
            Expr::Dict(values) => Expr::Dict(
                values
                    .iter()
                    .map(|(k, v)| (k.clone(), self.reference_value(v)))
                    .collect(),
            ),
            Expr::Mapping(values) => Expr::Mapping(
                values
                    .iter()
                    .map(|(k, v)| (self.reference_value(k), self.reference_value(v)))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }

    fn maps_table(&self, class: &Class, table: &TableIdentity) -> bool {
        if !self.proof_step() {
            return false;
        }
        let name = format!("{}.{}", class.module, class.name);
        let cached = self.resolved_classes.borrow();
        if let Some(summary) = cached
            .get(&(name.clone(), false, 127))
            .or_else(|| cached.get(&(name, true, 127)))
        {
            return summary.model
                && !summary.abstract_model
                && !summary.identity_unknown
                && summary.table.as_deref() == Some(&table.name)
                && summary.schema == table.schema;
        }
        drop(cached);
        let summary = self.resolve_class(class, &mut HashSet::new(), false);
        summary.model
            && !summary.abstract_model
            && !summary.identity_unknown
            && summary.unknown.is_empty()
            && summary.table.as_deref() == Some(&table.name)
            && summary.schema == table.schema
    }

    fn relationship(&self, value: &Expr, annotation: &Expr) -> Option<TableReferences> {
        let Expr::Call(_, args, kwargs) = value else {
            return None;
        };
        if !args.is_empty()
            || kwargs
                .keys()
                .any(|key| !matches!(key.as_str(), "secondary" | "lazy" | "viewonly" | "order_by"))
            || kwargs
                .get("lazy")
                .is_some_and(|v| !matches!(v, Expr::String(s) if s == "raise"))
            || kwargs
                .get("viewonly")
                .is_some_and(|v| !matches!(v, Expr::Bool(true)))
        {
            return None;
        }
        let Expr::Subscript(mapped, args) = annotation else {
            return None;
        };
        if !matches!(&**mapped, Expr::Name(name) if name == "sqlalchemy.orm.Mapped")
            || args.len() != 1
        {
            return None;
        }
        let Expr::Subscript(list, targets) = &args[0] else {
            return None;
        };
        if !matches!(&**list, Expr::Name(name) if name == "list") || targets.len() != 1 {
            return None;
        }
        let Expr::Name(target) = &targets[0] else {
            return None;
        };
        let target_class = self.classes.get(target).filter(|c| c.audited)?;
        let resolved = self.resolve_class(target_class, &mut HashSet::new(), false);
        if !resolved.model
            || resolved.abstract_model
            || resolved.identity_unknown
            || !resolved.unknown.is_empty()
        {
            return None;
        }
        let table = TableIdentity {
            name: resolved.table?,
            schema: resolved.schema,
        };
        if let Some(order) = kwargs.get("order_by") {
            let Expr::Name(name) = order else {
                return None;
            };
            let (owner, column) = name.rsplit_once('.')?;
            if owner != target || !resolved.mapped_attributes.contains(column) {
                return None;
            }
        }
        let mut references = TableReferences {
            targets: vec![table],
            unknown: vec![],
        };
        if let Some(secondary) = kwargs.get("secondary") {
            let Expr::String(name) = secondary else {
                return None;
            };
            let parts: Vec<_> = name.split('.').collect();
            if !(1..=2).contains(&parts.len())
                || parts.iter().any(|part| {
                    part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
            {
                return None;
            }
            references.targets.push(TableIdentity {
                name: parts.last()?.to_string(),
                schema: (parts.len() == 2).then(|| parts[0].to_string()),
            });
        }
        for table in &references.targets {
            let count = self
                .classes
                .values()
                .filter(|c| c.audited)
                .filter(|candidate| self.maps_table(candidate, table))
                .take(2)
                .count();
            if count != 1 {
                return None;
            }
        }
        Some(references)
    }

    fn mapping_expression(&self, value: &Expr, module: &str) -> bool {
        match value {
            Expr::Name(_) => self.symbol(value, module, 0).is_some_and(|name| {
                ((name.starts_with("sqlalchemy.") || !name.contains('.'))
                    && matches!(
                        name.rsplit('.').next(),
                        Some(
                            "Mapped"
                                | "Column"
                                | "mapped_column"
                                | "relationship"
                                | "column_property"
                                | "synonym"
                                | "declared_attr"
                                | "Table"
                                | "ForeignKey"
                                | "ForeignKeyConstraint"
                        )
                    ))
                    || name.starts_with("tortoise.fields.")
            }),
            Expr::Call(f, args, kwargs) => {
                self.mapping_expression(f, module)
                    || args
                        .iter()
                        .chain(kwargs.values())
                        .any(|v| self.mapping_expression(v, module))
            }
            Expr::Subscript(f, args) => {
                self.mapping_expression(f, module)
                    || args.iter().any(|v| self.mapping_expression(v, module))
            }
            Expr::Tuple(values) | Expr::Union(values) | Expr::Add(values) => {
                values.iter().any(|v| self.mapping_expression(v, module))
            }
            Expr::Dict(values) => values.values().any(|v| self.mapping_expression(v, module)),
            Expr::Mapping(values) => values.iter().any(|(k, v)| {
                self.mapping_expression(k, module) || self.mapping_expression(v, module)
            }),
            _ => false,
        }
    }

    fn constructor_names(&self, value: &Expr, module: &str, names: &mut Vec<String>) {
        match value {
            Expr::Name(name) => {
                let local = format!("{module}.{name}");
                if self.aliases.contains_key(&local) || self.classes.contains_key(&local) {
                    names.push(local);
                } else if self.aliases.contains_key(name)
                    || self.classes.contains_key(name)
                    || name.contains('.')
                {
                    names.push(name.clone());
                }
            }
            Expr::Call(f, args, kwargs) => {
                self.constructor_names(f, module, names);
                for value in args.iter().chain(kwargs.values()) {
                    self.constructor_names(value, module, names);
                }
            }
            Expr::Subscript(f, args) => {
                self.constructor_names(f, module, names);
                for value in args {
                    self.constructor_names(value, module, names);
                }
            }
            Expr::Tuple(values) | Expr::Union(values) | Expr::Add(values) => {
                for value in values {
                    self.constructor_names(value, module, names);
                }
            }
            Expr::Dict(values) => {
                for value in values.values() {
                    self.constructor_names(value, module, names);
                }
            }
            Expr::Mapping(values) => {
                for (key, value) in values {
                    self.constructor_names(key, module, names);
                    self.constructor_names(value, module, names);
                }
            }
            _ => {}
        }
    }

    fn proof_step(&self) -> bool {
        let left = self.proof_work.get();
        self.proof_work.set(left.saturating_sub(1));
        left > 0
    }

    fn data_type(&self, value: &Expr, visiting: &mut HashSet<String>) -> bool {
        if !self.proof_step() {
            return false;
        }
        match value {
            Expr::Name(name) => {
                matches!(
                    name.as_str(),
                    "str"
                        | "int"
                        | "float"
                        | "bool"
                        | "bytes"
                        | "object"
                        | "dict"
                        | "list"
                        | "tuple"
                        | "set"
                        | "typing.Any"
                        | "typing.Self"
                ) || self
                    .classes
                    .get(name)
                    .is_some_and(|class| self.data_class(class, visiting))
            }
            Expr::None => true,
            Expr::Union(values) => values.iter().all(|v| self.data_type(v, visiting)),
            Expr::Subscript(inner, args) => {
                matches!(&**inner, Expr::Name(name) if matches!(name.as_str(), "dict" | "list" | "tuple" | "set" | "typing.Optional" | "typing.Union" | "typing.ClassVar"))
                    && !args.is_empty()
                    && args.iter().all(|v| self.data_type(v, visiting))
            }
            _ => false,
        }
    }

    fn inert_value(&self, value: &Expr, module: &str, visiting: &mut HashSet<String>) -> bool {
        if !self.proof_step() {
            return false;
        }
        match value {
            Expr::String(_) | Expr::Bool(_) | Expr::Literal | Expr::None => true,
            Expr::Tuple(values) => values.iter().all(|v| self.inert_value(v, module, visiting)),
            Expr::Dict(values) => values
                .values()
                .all(|v| self.inert_value(v, module, visiting)),
            Expr::Mapping(values) => values.iter().all(|(k, v)| {
                self.inert_value(k, module, visiting) && self.inert_value(v, module, visiting)
            }),
            Expr::Call(function, args, kwargs) => {
                let Some(name) = self.symbol(function, module, 0) else {
                    return false;
                };
                let Some(class) = self.classes.get(&name) else {
                    return false;
                };
                if class.bases.len() != 1
                    || self.symbol(&class.bases[0], &class.module, 0).as_deref()
                        != Some("typing.NamedTuple")
                    || !self.data_class(class, visiting)
                    || args.len() > class.attrs.len()
                    || kwargs.contains_key("**")
                {
                    return false;
                }
                let mut fields = HashSet::new();
                for attr in class.attrs.iter().take(args.len()) {
                    fields.insert(attr.name.as_str());
                }
                for key in kwargs.keys() {
                    if !class.attrs.iter().any(|a| &a.name == key) || !fields.insert(key.as_str()) {
                        return false;
                    }
                }
                class
                    .attrs
                    .iter()
                    .all(|a| fields.contains(a.name.as_str()) || !matches!(a.value, Expr::Missing))
                    && args
                        .iter()
                        .chain(kwargs.values())
                        .all(|v| self.inert_value(v, module, visiting))
            }
            _ => false,
        }
    }

    fn inert_metadata(
        &self,
        class: &Class,
        attr: &Attribute,
        annotation: &Expr,
        value: &Expr,
    ) -> bool {
        // ORM construction/mapping protocols never become application metadata.
        if matches!(
            attr.name.as_str(),
            "metadata"
                | "registry"
                | "type_annotation_map"
                | "__allow_unmapped__"
                | "__doc__"
                | "__module__"
                | "__qualname__"
                | "__match_args__"
                | "__mro_entries__"
                | "__orig_bases__"
                | "__classcell__"
                | "__bases__"
                | "__annotations__"
                | "__slots__"
                | "__dict__"
                | "__tablename__"
                | "__table_args__"
                | "__table__"
                | "__mapper_args__"
                | "__abstract__"
                | "__mapper__"
                | "__table_cls__"
                | "__mapper_cls__"
                | "__declare_first__"
                | "__declare_last__"
                | "__sa_decl_prepare__"
                | "__sa_decl_prepare_nocascade__"
                | "__init__"
                | "__new__"
                | "__init_subclass__"
                | "__class_getitem__"
                | "__set_name__"
        ) || (class.orm == ModelOrm::Tortoise && attr.name == "Meta")
            || attr.name.starts_with("_sa_")
            || attr.name.starts_with("__sa_")
        {
            return false;
        }
        (matches!(annotation, Expr::Name(name) if name == "typing.ClassVar")
            || (matches!(annotation, Expr::Subscript(inner, args) if matches!(&**inner, Expr::Name(name) if name == "typing.ClassVar") && args.len() == 1)
                && self.data_type(annotation, &mut HashSet::new())))
            && self.inert_value(value, &class.module, &mut HashSet::new())
    }

    fn data_class(&self, class: &Class, visiting: &mut HashSet<String>) -> bool {
        let key = format!("{}.{}", class.module, class.name);
        if !self.proof_step() || visiting.len() >= 32 || !visiting.insert(key.clone()) {
            return false;
        }
        let cache_key = (key.clone(), 32 - visiting.len());
        if self.inert_records.borrow().contains(&cache_key) {
            visiting.remove(&key);
            return true;
        }
        if self.mutated.contains(&key) {
            visiting.remove(&key);
            return false;
        }
        let record = class.bases.len() == 1
            && self.symbol(&class.bases[0], &class.module, 0).as_deref()
                == Some("typing.NamedTuple");
        let result = class.data_body
            && class.meta.is_empty()
            && (record && class.decorators.is_empty() && !class.has_methods)
            && class
                .attrs
                .iter()
                .map(|a| &a.name)
                .collect::<HashSet<_>>()
                .len()
                == class.attrs.len()
            && class.attrs.iter().all(|a| {
                !a.name.starts_with('_')
                    && self.data_type(&self.expand(&a.annotation, &class.module, 0), visiting)
                    && (matches!(a.value, Expr::Missing)
                        || self.inert_value(
                            &self.expand(&a.value, &class.module, 0),
                            &class.module,
                            visiting,
                        ))
            });
        visiting.remove(&key);
        if result {
            self.inert_records.borrow_mut().insert(cache_key);
        }
        result
    }

    fn read(&mut self, file: ModelFile, selected: Option<&str>) {
        let package = file.path.file_name().is_some_and(|n| n == "__init__.py");
        let module = module(&file.path);
        if let Some((path, orm)) = self.imported_modules.get(&module) {
            if path != &file.path || *orm != file.orm {
                self.unknown
                    .push(format!("{module}: ambiguous module/package source"));
            }
            return;
        }
        self.imported_modules
            .insert(module.clone(), (file.path.clone(), file.orm));

        let parsed = match crate::parser::ParsedMigration::parse(file.source.clone()) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.unknown
                    .push(format!("{}: {error}", file.path.display()));
                return;
            }
        };
        if parsed.has_errors() {
            self.unknown
                .push(format!("{}: invalid Python source", file.path.display()));
            return;
        }
        for node in named(parsed.root_node()) {
            if namespace_mutation(node, &file.source, None) {
                self.unknown
                    .push(format!("{module}: dynamic module namespace"));
            }
            let definition = if node.kind() == "decorated_definition" {
                named(node)
                    .into_iter()
                    .find(|n| matches!(n.kind(), "function_definition" | "class_definition"))
            } else {
                Some(node)
            };
            if definition.is_some_and(|n| {
                matches!(n.kind(), "function_definition" | "class_definition")
                    && n.child_by_field_name("name").is_some_and(|name| {
                        RESERVED_BINDINGS.contains(&text(name, &file.source))
                            && text(name, &file.source) != "__getattr__"
                    })
            }) {
                self.unknown.push(format!(
                    "{module}: reserved definition binding is unresolved"
                ));
            }
        }
        let mut pending = if selected.is_none() {
            vec![(parsed.root_node(), false)]
        } else {
            vec![]
        };
        while let Some((node, in_function)) = pending.pop() {
            if in_function && node.kind() == "class_definition" {
                self.unknown.push(format!(
                    "{}:{}: model class nested in function is unsupported",
                    file.path.display(),
                    node.start_position().row + 1
                ));
            }
            let nested_function = in_function || node.kind() == "function_definition";
            pending.extend(
                named(node)
                    .into_iter()
                    .map(|child| (child, nested_function)),
            );
        }
        for node in named(parsed.root_node()) {
            match node.kind() {
                "import_from_statement" | "import_statement" => {
                    self.import(node, &file.source, &module, package);
                }
                "class_definition" => {
                    self.read_class(node, &file, &module, vec![], selected.is_none())
                }
                "decorated_definition" => {
                    if let Some(class) = named(node)
                        .into_iter()
                        .find(|n| n.kind() == "class_definition")
                    {
                        let decorators = named(node)
                            .into_iter()
                            .filter(|n| n.kind() == "decorator")
                            .map(|n| {
                                named(n)
                                    .first()
                                    .map_or(Expr::Other, |n| expr(*n, &file.source))
                            })
                            .collect();
                        self.read_class(class, &file, &module, decorators, selected.is_none());
                    } else {
                        if let Some(function) = named(node)
                            .into_iter()
                            .find(|n| n.kind() == "function_definition")
                        {
                            if let Some(name) = function.child_by_field_name("name") {
                                let key = format!("{module}.{}", text(name, &file.source));
                                if self.aliases.contains_key(&key)
                                    || self.classes.contains_key(&key)
                                {
                                    self.ambiguous.insert(key.clone());
                                }
                                self.aliases.insert(key, (Expr::Other, module.clone()));
                            }
                        }
                    }
                }
                "expression_statement" => {
                    if named(node).len() != 1 {
                        self.unknown.push(format!(
                            "{}:{}: unsupported multi-expression module statement",
                            file.path.display(),
                            node.start_position().row + 1
                        ));
                        continue;
                    }
                    if let Some(assignment) =
                        named(node).first().filter(|n| n.kind() == "assignment")
                    {
                        if let (Some(left), Some(right)) = (
                            assignment.child_by_field_name("left"),
                            assignment.child_by_field_name("right"),
                        ) {
                            if left.kind() == "identifier" {
                                if RESERVED_BINDINGS.contains(&text(left, &file.source)) {
                                    self.unknown.push(format!(
                                        "{}: dynamic module namespace",
                                        file.path.display()
                                    ));
                                }
                                let value = expr(right, &file.source);
                                if let Expr::Call(function, _, _) = &value {
                                    let name =
                                        self.symbol(function, &module, 0).unwrap_or_default();
                                    if name == "sqlalchemy.Table"
                                        || name.ends_with(".map_imperatively")
                                        || name.ends_with(".automap_base")
                                    {
                                        self.unknown.push(format!(
                                            "{}:{}: unsupported imperative/reflected mapping",
                                            file.path.display(),
                                            node.start_position().row + 1
                                        ));
                                    }
                                }
                                let key = format!("{module}.{}", text(left, &file.source));
                                if self.aliases.contains_key(&key)
                                    || self.classes.contains_key(&key)
                                {
                                    self.ambiguous.insert(key.clone());
                                }
                                self.aliases.insert(key, (value, module.clone()));
                            } else if selected.is_none() {
                                self.unknown.push(format!(
                                    "{}:{}: dynamic attribute assignment",
                                    file.path.display(),
                                    node.start_position().row + 1
                                ));
                            }
                        }
                    }
                }
                "if_statement" | "for_statement" | "try_statement" | "with_statement"
                | "while_statement" | "match_statement" => {
                    if selected.is_none() {
                        self.unknown.push(format!(
                            "{}:{}: conditional/dynamic module definitions",
                            file.path.display(),
                            node.start_position().row + 1
                        ));
                    } else {
                        let keys: Vec<_> = self
                            .aliases
                            .keys()
                            .chain(self.classes.keys())
                            .filter(|key| key.starts_with(&format!("{module}.")))
                            .cloned()
                            .collect();
                        for key in keys {
                            if mentions_binding(
                                node,
                                &file.source,
                                key.rsplit('.').next().unwrap_or(""),
                            ) {
                                self.unknown.push(format!(
                                    "{}: conditional binding {key}",
                                    file.path.display()
                                ));
                            }
                        }
                        if selected.is_some_and(|key| {
                            mentions_binding(
                                node,
                                &file.source,
                                key.rsplit('.').next().unwrap_or(""),
                            )
                        }) {
                            self.unknown.push(format!(
                                "{}: conditional required definition",
                                file.path.display()
                            ));
                        }
                    }
                }
                "function_definition" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let key = format!("{module}.{}", text(name, &file.source));
                        if self.aliases.contains_key(&key) || self.classes.contains_key(&key) {
                            self.ambiguous.insert(key.clone());
                        }
                        self.aliases.insert(key, (Expr::Other, module.clone()));
                    }
                }
                "comment" | "pass_statement" => {}
                _ => self.unknown.push(format!(
                    "{}:{}: unsupported executable module construct",
                    file.path.display(),
                    node.start_position().row + 1
                )),
            }
        }
        let mut mutations = vec![parsed.root_node()];
        while let Some(node) = mutations.pop() {
            if node.kind() == "assignment" {
                if let (Some(left), Some(right)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) {
                    let mut bindings = Vec::new();
                    assignment_names(left, &file.source, false, &mut bindings);
                    let mut targets = Vec::new();
                    alias_targets(&expr(right, &file.source), &mut targets);
                    for binding in bindings {
                        for target in &targets {
                            let target = if matches!(
                                target.as_str(),
                                "setattr" | "delattr" | "exec" | "eval"
                            ) {
                                target.clone()
                            } else {
                                format!("{module}.{target}")
                            };
                            self.mutation_aliases
                                .push((format!("{module}.{binding}"), target));
                        }
                    }
                }
            }
            if matches!(
                node.kind(),
                "assignment" | "augmented_assignment" | "delete_statement"
            ) {
                if let Some(target) = node
                    .child_by_field_name("left")
                    .or_else(|| named(node).first().copied())
                {
                    let mut roots = Vec::new();
                    assignment_names(target, &file.source, true, &mut roots);
                    self.mutated
                        .extend(roots.into_iter().map(|name| format!("{module}.{name}")));
                }
            }
            mutations.extend(named(node));
        }
        {
            let mut imported_names = Vec::new();
            let mut imports = vec![parsed.root_node()];
            while let Some(node) = imports.pop() {
                if matches!(node.kind(), "import_statement" | "import_from_statement") {
                    let mut context = Sources::default();
                    context.import(node, &file.source, &module, package);
                    for (name, (target, _)) in context.aliases {
                        if let Expr::Name(target) = target {
                            imported_names
                                .push((name.rsplit('.').next().unwrap_or("").to_owned(), target));
                        }
                    }
                }
                imports.extend(named(node));
            }
            let mut pending = vec![(parsed.root_node(), false)];
            while let Some((node, deferred)) = pending.pop() {
                if node.kind() == "call" {
                    if let Some(function) = node.child_by_field_name("function") {
                        let raw = text(function, &file.source);
                        let symbol = self
                            .symbol(&expr(function, &file.source), &module, 0)
                            .unwrap_or_default();
                        let mutation_call = matches!(
                            symbol.as_str(),
                            "setattr"
                                | "delattr"
                                | "exec"
                                | "eval"
                                | "builtins.setattr"
                                | "builtins.delattr"
                                | "builtins.exec"
                                | "builtins.eval"
                        );
                        if let Expr::Name(name) = expr(function, &file.source) {
                            let name = format!("{module}.{name}");
                            if self.mutation_aliases.iter().any(|(alias, _)| {
                                name == *alias
                                    || name
                                        .strip_prefix(alias)
                                        .is_some_and(|suffix| suffix.starts_with('.'))
                            }) {
                                self.mutation_calls.insert(name);
                            }
                        }
                        let explicit = matches!(
                            symbol.as_str(),
                            "sqlalchemy.Table" | "sqlalchemy.schema.Table"
                        ) || (symbol == "type"
                            && node
                                .child_by_field_name("arguments")
                                .is_some_and(|args| named(args).len() > 1))
                            || [
                                "map_imperatively",
                                "automap_base",
                                "mapped",
                                "mapped_as_dataclass",
                            ]
                            .iter()
                            .any(|name| {
                                raw.rsplit('.').next() == Some(name)
                                    || symbol.rsplit('.').next() == Some(name)
                            })
                            || (deferred
                                && self.mapping_expression(&expr(function, &file.source), &module));
                        let imported_mapping = imported_names.iter().any(|(binding, target)| {
                            let target = if raw == binding {
                                target.clone()
                            } else if let Some(suffix) = raw.strip_prefix(&format!("{binding}.")) {
                                format!("{target}.{suffix}")
                            } else {
                                return false;
                            };
                            matches!(
                                target.as_str(),
                                "builtins.setattr"
                                    | "builtins.delattr"
                                    | "builtins.exec"
                                    | "builtins.eval"
                            ) || (selected.is_none()
                                && (matches!(
                                    target.as_str(),
                                    "sqlalchemy.Table" | "sqlalchemy.schema.Table"
                                ) || (deferred
                                    && self.mapping_expression(&Expr::Name(target), ""))))
                        });
                        if mutation_call || (explicit && selected.is_none()) || imported_mapping {
                            self.unknown.push(format!(
                                "{}:{}: unsupported explicit ORM declaration",
                                file.path.display(),
                                node.start_position().row + 1
                            ));
                        }
                    }
                }
                let deferred = deferred || matches!(node.kind(), "function_definition" | "lambda");
                pending.extend(named(node).into_iter().map(|child| (child, deferred)));
            }
        }
    }
    fn static_scalar(&self, value: &Expr, module: &str) -> bool {
        match self.expand(value, module, 0) {
            Expr::String(_) | Expr::Literal => true,
            Expr::Name(name) => matches!(name.as_str(), "string.ascii_letters" | "string.digits"),
            Expr::Add(values) => values.iter().all(|v| self.static_scalar(v, module)),
            Expr::Call(function, args, kwargs) => {
                self.symbol(&function, module, 0).as_deref() == Some("len")
                    && args.len() == 1
                    && kwargs.is_empty()
                    && self.static_scalar(&args[0], module)
            }
            _ => false,
        }
    }
    fn dynamic_mapping_value(&self, value: &Expr, module: &str) -> bool {
        match value {
            Expr::Call(function, args, kwargs) => {
                let name = self.symbol(function, module, 0).unwrap_or_default();
                if name == "len" {
                    return args.len() != 1
                        || !kwargs.is_empty()
                        || !self.static_scalar(&args[0], module);
                }
                let known = matches!(
                    name.as_str(),
                    "sqlalchemy.orm.registry"
                        | "sqlalchemy.orm.declarative_base"
                        | "sqlalchemy.ext.declarative.declarative_base"
                        | "sqlalchemy.orm.mapped_column"
                        | "sqlalchemy.Column"
                        | "sqlalchemy.Identity"
                        | "sqlalchemy.Sequence"
                        | "sqlalchemy.Computed"
                        | "sqlalchemy.ForeignKey"
                        | "contextvars.ContextVar"
                        | "typing.TypeVar"
                        | "typing.NewType"
                        | "typing.TypeVarTuple"
                        | "typing.ParamSpec"
                        | "dict"
                        | "list"
                        | "tuple"
                        | "set"
                        | "frozenset"
                ) || matches!(
                    name.strip_prefix("sqlalchemy."),
                    Some(
                        "BigInteger"
                            | "Integer"
                            | "String"
                            | "Text"
                            | "Boolean"
                            | "DateTime"
                            | "LargeBinary"
                            | "UUID"
                            | "Float"
                            | "Numeric"
                    )
                ) || name.starts_with("sqlalchemy.dialects.");
                !known
                    || args.iter().any(|a| self.dynamic_mapping_value(a, module))
                    || kwargs
                        .values()
                        .any(|a| self.dynamic_mapping_value(a, module))
            }
            Expr::Tuple(values) | Expr::Union(values) => {
                values.iter().any(|a| self.dynamic_mapping_value(a, module))
            }
            Expr::Dict(values) => values
                .values()
                .any(|a| self.dynamic_mapping_value(a, module)),
            Expr::Mapping(values) => values.iter().any(|(key, value)| {
                self.dynamic_mapping_value(key, module) || self.dynamic_mapping_value(value, module)
            }),
            Expr::Subscript(inner, values) => {
                self.dynamic_mapping_value(inner, module)
                    || values.iter().any(|a| self.dynamic_mapping_value(a, module))
            }
            Expr::Add(_) => !self.static_scalar(value, module),
            Expr::Other | Expr::Limit => true,
            _ => false,
        }
    }
    fn import(&mut self, node: Node<'_>, source: &str, module: &str, package: bool) {
        let raw = text(node, source).replace(['(', ')', '\n'], " ");
        if let Some(rest) = raw.strip_prefix("from ") {
            let Some((origin, names)) = rest.split_once(" import ") else {
                self.unknown.push("unresolved Python import".into());
                return;
            };
            let origin = origin.trim();
            let origin = if origin.starts_with('.') {
                let dots = origin.chars().take_while(|c| *c == '.').count();
                let mut parts: Vec<_> = module.split('.').collect();
                let remove = dots - usize::from(package);
                for _ in 0..remove {
                    parts.pop();
                }
                let suffix = origin.trim_start_matches('.');
                if !suffix.is_empty() {
                    parts.push(suffix);
                }
                parts.join(".")
            } else {
                origin.to_owned()
            };
            for entry in names.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let parts: Vec<_> = entry.split_whitespace().collect();
                if parts[0] == "*" {
                    self.unknown.push(format!(
                        "{module}: wildcard import prevents complete mapping resolution"
                    ));
                    continue;
                }
                let alias = if parts.len() == 3 { parts[2] } else { parts[0] };
                if RESERVED_BINDINGS.contains(&alias) {
                    self.unknown
                        .push(format!("{module}: reserved import binding is unresolved"));
                }
                let key = format!("{module}.{alias}");
                if self.aliases.contains_key(&key) || self.classes.contains_key(&key) {
                    self.ambiguous.insert(key.clone());
                }
                self.aliases.insert(
                    key,
                    (Expr::Name(format!("{origin}.{}", parts[0])), String::new()),
                );
            }
        } else if let Some(rest) = raw.strip_prefix("import ") {
            for entry in rest.split(',') {
                let parts: Vec<_> = entry.split_whitespace().collect();
                if parts.is_empty() {
                    continue;
                }
                let (alias, target) = if parts.len() == 3 {
                    (parts[2], parts[0])
                } else {
                    (
                        parts[0].split('.').next().unwrap_or(parts[0]),
                        parts[0].split('.').next().unwrap_or(parts[0]),
                    )
                };
                if RESERVED_BINDINGS.contains(&alias) {
                    self.unknown
                        .push(format!("{module}: reserved import binding is unresolved"));
                }
                let key = format!("{module}.{alias}");
                if self.aliases.contains_key(&key) || self.classes.contains_key(&key) {
                    self.ambiguous.insert(key.clone());
                }
                self.aliases
                    .insert(key, (Expr::Name(target.into()), String::new()));
            }
        }
    }
    fn read_class(
        &mut self,
        node: Node<'_>,
        file: &ModelFile,
        module: &str,
        decorators: Vec<Expr>,
        audited: bool,
    ) {
        let (Some(name), Some(body)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("body"),
        ) else {
            return;
        };
        let (attrs, mut unsupported) = attrs(body, &file.source, &file.path);
        let mut meta = vec![];
        for nested in named(body)
            .into_iter()
            .filter(|n| n.kind() == "class_definition")
        {
            if nested
                .child_by_field_name("name")
                .is_some_and(|n| text(n, &file.source) == "Meta")
            {
                if let Some(body) = nested.child_by_field_name("body") {
                    let (meta_attrs, meta_unsupported) = attrs_fn(body, &file.source, &file.path);
                    meta = meta_attrs;
                    let meta_unsupported = meta_unsupported
                        || nested.child_by_field_name("superclasses").is_some()
                        || named(body).iter().any(|node| {
                            matches!(node.kind(), "class_definition" | "decorated_definition")
                        });
                    unsupported |= meta_unsupported;
                    if meta_unsupported && audited {
                        self.unknown.push(format!(
                            "{}:{}: dynamic nested Meta class",
                            file.path.display(),
                            nested.start_position().row + 1
                        ));
                    }
                }
            } else {
                unsupported = true;
            }
        }
        let bases = node
            .child_by_field_name("superclasses")
            .map_or(vec![], |n| {
                named(n).iter().map(|n| expr(*n, &file.source)).collect()
            });
        let key = format!("{module}.{}", text(name, &file.source));
        if self.aliases.remove(&key).is_some() {
            self.ambiguous.insert(key.clone());
        }
        let class = Class {
            name: text(name, &file.source).into(),
            module: module.into(),
            orm: file.orm,
            attrs,
            meta,
            bases,
            unsupported,
            decorators,
            has_methods: named(body)
                .iter()
                .any(|n| matches!(n.kind(), "function_definition" | "decorated_definition")),
            data_body: data_body(body),
            audited,
            location: format!("{}:{}", file.path.display(), node.start_position().row + 1),
        };
        if self.classes.insert(key.clone(), class).is_some() {
            self.ambiguous.insert(key);
        }
    }
    fn symbol(&self, value: &Expr, module: &str, depth: usize) -> Option<String> {
        if depth > 32 {
            return None;
        }
        let Expr::Name(name) = value else {
            return None;
        };
        let key = (name.clone(), module.to_owned(), depth);
        if self.cache_symbols {
            if let Some(result) = self.symbol_cache.borrow().get(&key) {
                return Some(result.clone());
            }
        }
        if !self.proof_step() {
            return None;
        }
        let result = (|| {
            let (head, suffix) = name
                .split_once('.')
                .map_or((name.as_str(), ""), |(h, s)| (h, s));
            let local = format!("{module}.{head}");
            if self.ambiguous.contains(&local) || self.ambiguous.contains(name) {
                return None;
            }
            if let Some((alias, origin)) = self.aliases.get(&local) {
                let target = self.symbol(alias, origin, depth + 1)?;
                return Some(if suffix.is_empty() {
                    target
                } else {
                    format!("{target}.{suffix}")
                });
            }
            if self.classes.contains_key(&local) {
                return Some(if suffix.is_empty() {
                    local
                } else {
                    format!("{local}.{suffix}")
                });
            }
            if let Some((alias, origin)) = self.aliases.get(name) {
                return self.symbol(alias, origin, depth + 1);
            }
            Some(name.clone())
        })();
        if self.cache_symbols {
            if let Some(result) = &result {
                self.symbol_cache.borrow_mut().insert(key, result.clone());
            }
        }
        result
    }
    fn expand(&self, value: &Expr, module: &str, depth: usize) -> Expr {
        self.expand_bounded(value, module, depth)
    }
    fn expand_bounded(&self, value: &Expr, module: &str, depth: usize) -> Expr {
        if depth > 32 || !self.proof_step() {
            return Expr::Limit;
        }
        match value {
            Expr::Name(name) => {
                if let Some((alias, origin)) = self
                    .aliases
                    .get(&format!("{module}.{name}"))
                    .or_else(|| self.aliases.get(name))
                {
                    return self.expand_bounded(alias, origin, depth + 1);
                }
                let symbol = self
                    .symbol(value, module, 0)
                    .unwrap_or_else(|| name.clone());
                if let Some((alias, origin)) = self.aliases.get(&symbol) {
                    return self.expand_bounded(alias, origin, depth + 1);
                }
                Expr::Name(symbol)
            }
            Expr::Call(function, args, kwargs) => Expr::Call(
                Box::new(self.expand_bounded(function, module, depth + 1)),
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1))
                    .collect(),
                kwargs
                    .iter()
                    .map(|(k, v)| (k.clone(), self.expand_bounded(v, module, depth + 1)))
                    .collect(),
            ),
            Expr::Dict(values) => Expr::Dict(
                values
                    .iter()
                    .map(|(k, v)| (k.clone(), self.expand_bounded(v, module, depth + 1)))
                    .collect(),
            ),
            Expr::Mapping(values) => Expr::Mapping(
                values
                    .iter()
                    .map(|(key, value)| {
                        (
                            self.expand_bounded(key, module, depth + 1),
                            self.expand_bounded(value, module, depth + 1),
                        )
                    })
                    .collect(),
            ),
            Expr::Add(args) => Expr::Add(
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1))
                    .collect(),
            ),
            Expr::Union(args) => Expr::Union(
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1))
                    .collect(),
            ),
            Expr::Tuple(args) => Expr::Tuple(
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1))
                    .collect(),
            ),
            Expr::Subscript(inner, args) => Expr::Subscript(
                Box::new(self.expand_bounded(inner, module, depth + 1)),
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }
    fn resolve_class(
        &self,
        class: &Class,
        visiting: &mut HashSet<String>,
        relationships: bool,
    ) -> Resolved {
        let key = format!("{}.{}", class.module, class.name);
        if !self.proof_step() {
            return Resolved {
                model: true,
                identity_unknown: true,
                unknown: vec![format!(
                    "{}: model resolution work limit exceeded",
                    class.location
                )],
                ..Resolved::default()
            };
        }
        if visiting.len() >= 128 || !visiting.insert(key.clone()) {
            return Resolved {
                model: true,
                unknown: vec![format!("{}: cyclic inheritance", class.location)],
                ..Resolved::default()
            };
        }
        let cache_key = (key.clone(), relationships, 128 - visiting.len());
        let cached = self.resolved_classes.borrow();
        if let Some(cached) = cached.get(&cache_key).or_else(|| {
            cached
                .get(&(key.clone(), !relationships, 128 - visiting.len()))
                .filter(|summary| !summary.has_relationships)
        }) {
            let cost = cached.copy_cost();
            let remaining = self.proof_work.get();
            visiting.remove(&key);
            if cost >= remaining {
                self.proof_work.set(0);
                return Resolved {
                    identity_unknown: true,
                    unknown: vec![format!(
                        "{}: model summary copy work limit exceeded",
                        class.location
                    )],
                    ..Resolved::default()
                };
            }
            self.proof_work.set(remaining - cost);
            return cached.clone();
        }
        drop(cached);
        let mut result = Resolved::default();
        if self.mutated.contains(&key) {
            visiting.remove(&key);
            return Resolved {
                eager_unknown: true,
                identity_unknown: true,
                unknown: vec![format!(
                    "{}: required class binding is mutated",
                    class.location
                )],
                ..Resolved::default()
            };
        }
        let dataclass = class.decorators.len() == 1
            && matches!(&class.decorators[0], Expr::Call(f, args, kw)
                if self.symbol(f, &class.module, 0).as_deref() == Some("dataclasses.dataclass") && args.is_empty() && kw.len() == 2
                    && matches!(kw.get("frozen"), Some(Expr::Bool(true))) && matches!(kw.get("slots"), Some(Expr::Bool(true))))
            && class
                .bases
                .iter()
                .all(|base| self.symbol(base, &class.module, 0).as_deref() == Some("object"));
        if !class.decorators.is_empty() && !dataclass {
            result.eager_unknown = true;
            result
                .unknown
                .push(format!("{}: unresolved class decorator", class.location));
        }

        for attr in &class.meta {
            if self.eager_effects(&self.expand(&attr.value, &class.module, 0))
                || self.eager_effects(&self.expand(&attr.annotation, &class.module, 0))
            {
                result.identity_unknown = true;
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: eager Meta expression is unresolved",
                    attr.location
                ));
            }
        }
        let mut terminal_base = false;
        for base in &class.bases {
            let expanded = self.expand(base, &class.module, 0);
            let static_factory = matches!(&expanded, Expr::Call(function, args, kwargs)
                if matches!(&**function, Expr::Name(name) if matches!(name.as_str(), "sqlalchemy.orm.declarative_base" | "sqlalchemy.ext.declarative.declarative_base"))
                    && args.is_empty() && kwargs.is_empty());
            if !static_factory && self.eager_effects(&expanded) {
                result.identity_unknown = true;
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: eager base expression is unresolved",
                    class.location
                ));
            }
            let symbol = self.symbol(base, &class.module, 0).unwrap_or_default();
            let terminal = match class.orm {
                ModelOrm::Sqlalchemy => matches!(
                    symbol.as_str(),
                    "sqlalchemy.orm.DeclarativeBase" | "sqlalchemy.orm.DeclarativeBaseNoMeta"
                ),
                ModelOrm::Tortoise => {
                    matches!(symbol.as_str(), "tortoise.models.Model" | "tortoise.Model")
                }
            };
            if terminal {
                result.model = true;
                terminal_base = true;
                continue;
            }
            if matches!(
                symbol.as_str(),
                "object"
                    | "Exception"
                    | "RuntimeError"
                    | "ValueError"
                    | "TypeError"
                    | "typing.NamedTuple"
                    | "typing.Protocol"
                    | "typing.TypedDict"
                    | "pydantic.BaseModel"
                    | "sqlalchemy.TypeDecorator"
                    | "sqlalchemy.types.TypeDecorator"
            ) {
                continue;
            }
            if let Some(parent) = self.classes.get(&symbol) {
                let inherited = self.resolve_class(parent, visiting, relationships);
                result.model |= inherited.model;
                result.has_relationships |= inherited.has_relationships;
                result.eager_unknown |= inherited.eager_unknown;
                result.table = inherited.table.or(result.table);
                result.schema = inherited.schema.or(result.schema);
                result.columns.extend(inherited.columns);
                result.mapped_attributes.extend(inherited.mapped_attributes);
                result.unknown.extend(inherited.unknown);
                result
                    .references
                    .targets
                    .extend(inherited.references.targets);
                result
                    .references
                    .unknown
                    .extend(inherited.references.unknown);
                result.primary_key |= inherited.primary_key;
                result.identity_unknown |= inherited.identity_unknown;
            } else {
                // Classic declarative_base() is also a supported terminal factory.
                let expanded = self.expand(base, &class.module, 0);
                if let Expr::Call(function, args, kwargs) = expanded {
                    let factory = self.symbol(&function, &class.module, 0).unwrap_or_default();
                    if matches!(
                        factory.as_str(),
                        "sqlalchemy.orm.declarative_base"
                            | "sqlalchemy.ext.declarative.declarative_base"
                    ) && args.is_empty()
                        && kwargs.is_empty()
                    {
                        result.model = true;
                        continue;
                    }
                }
                result.model = true;
                result
                    .unknown
                    .push(format!("{}: unresolved base {symbol}", class.location));
            }
        }
        let mapping = !result.model
            && (class.attrs.iter().any(|attr| {
                matches!(
                    attr.name.as_str(),
                    "__tablename__"
                        | "__table__"
                        | "registry"
                        | "__mapper_args__"
                        | "type_annotation_map"
                ) || self.mapping_expression(&attr.value, &class.module)
                    || self.mapping_expression(&attr.annotation, &class.module)
            }) || !class.meta.is_empty());
        if !result.model && !mapping {
            if class.unsupported && !dataclass {
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: unsupported class-body construct or decorator",
                    class.location
                ));
            }
            visiting.remove(&key);
            return result;
        }
        if dataclass && mapping {
            result.eager_unknown = true;
            result.unknown.push(format!(
                "{}: mapping-bearing class has no supported ORM ancestry",
                class.location
            ));
        }
        if !class.audited {
            result.identity_unknown = true;
            result.eager_unknown = true;
            result.unknown.push(format!(
                "{}: dependency is outside audited model sources",
                class.location
            ));
        }
        if terminal_base && class.orm == ModelOrm::Sqlalchemy && result.table.is_none() {
            result.abstract_model = true;
        }
        let own_table = class.attrs.iter().any(|a| a.name == "__tablename__")
            || class.meta.iter().any(|a| a.name == "table");
        // Table metadata alone does not establish ORM registration.
        if own_table {
            result.abstract_model = false;
        }
        for attr in &class.attrs {
            let annotation = self.expand(&attr.annotation, &class.module, 0);
            if class.orm == ModelOrm::Sqlalchemy
                && (unsafe_forward_annotation(&attr.annotation)
                    || unsafe_forward_annotation(&annotation))
            {
                result.identity_unknown = true;
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: executable forward annotation is unresolved",
                    attr.location
                ));
                continue;
            }
            if attr.name.starts_with("__")
                && !matches!(attr.annotation, Expr::Missing)
                && !matches!(&annotation, Expr::Name(name) if name == "typing.ClassVar")
                && !self.data_type(&annotation, &mut HashSet::new())
            {
                result.identity_unknown = true;
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: mapping annotation is not proven inert",
                    attr.location
                ));
                continue;
            }
            let expanded = self.expand(&attr.value, &class.module, 0);
            if self.inert_metadata(class, attr, &annotation, &expanded)
                || (attr.name == "registry"
                    && self.registry(&attr.value, &class.module)
                    && (matches!(attr.annotation, Expr::Missing)
                        || matches!(&annotation, Expr::Name(name) if name == "typing.ClassVar")))
            {
                result.mapped_attributes.remove(&attr.name);
                continue;
            }
            if self.eager_effects(&expanded) || self.eager_effects(&annotation) {
                result.identity_unknown = true;
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: eager attribute expression is unresolved",
                    attr.location
                ));
                continue;
            }
            if matches!(&expanded, Expr::Call(f, _, _) if self.symbol(f, &class.module, 0).as_deref() == Some("sqlalchemy.orm.relationship"))
            {
                result.has_relationships = true;
                if relationships {
                    if let Some(references) = self.relationship(&expanded, &annotation) {
                        result.references.targets.extend(references.targets);
                    } else {
                        result.references.unknown.push(format!(
                            "{}: relationship evidence is unresolved",
                            attr.location
                        ));
                        // Column evidence retains its existing secondary-table guard;
                        // unresolved endpoint references belong to table retirement.
                        self.field(class, attr, annotation, expanded, &mut result);
                    }
                }
                continue;
            }
            result.references.collect(&self.reference_value(&expanded));
            result
                .references
                .collect(&self.reference_value(&annotation));
            match attr.name.as_str() {
                "__tablename__" => match &attr.value {
                    Expr::String(value) => result.table = Some(value.clone()),
                    _ => {
                        result.identity_unknown = true;
                        result
                            .unknown
                            .push(format!("{}: dynamic table name", attr.location));
                    }
                },
                "__abstract__" => {
                    if let Expr::Bool(value) = attr.value {
                        result.abstract_model = value;
                    } else {
                        result
                            .unknown
                            .push(format!("{}: dynamic abstract flag", attr.location));
                    }
                }
                "__table_args__" => {
                    // An own __table_args__ replaces inherited options, including schema.
                    result.schema = None;
                    let mut values = vec![];
                    match &expanded {
                        Expr::Dict(items) => values.push(items),
                        Expr::Tuple(items) => {
                            for item in items {
                                match item {
                                    Expr::Dict(items) => values.push(items),
                                    Expr::Call(function, _, _)
                                        if self.symbol(function, &class.module, 0).is_some_and(
                                            |s| {
                                                matches!(
                                                    s.as_str(),
                                                    "sqlalchemy.Index"
                                                        | "sqlalchemy.UniqueConstraint"
                                                        | "sqlalchemy.CheckConstraint"
                                                        | "sqlalchemy.ForeignKeyConstraint"
                                                        | "sqlalchemy.PrimaryKeyConstraint"
                                                )
                                            },
                                        ) => {}
                                    _ => result.unknown.push(format!(
                                        "{}: unsupported table metadata",
                                        attr.location
                                    )),
                                }
                            }
                        }
                        _ => {
                            result.identity_unknown = true;
                            result
                                .unknown
                                .push(format!("{}: unsupported table metadata", attr.location));
                        }
                    }
                    for values in values {
                        if let Some(schema) = values.get("schema") {
                            match schema {
                                Expr::String(schema) => result.schema = Some(schema.clone()),
                                _ => {
                                    result.identity_unknown = true;
                                    result
                                        .unknown
                                        .push(format!("{}: dynamic schema", attr.location));
                                }
                            }
                        }
                        if values.contains_key("autoload_with") || values.contains_key("autoload") {
                            result
                                .unknown
                                .push(format!("{}: reflected table mapping", attr.location));
                        }
                    }
                }
                "__table__" | "metadata" | "__mapper_args__" | "type_annotation_map" => {
                    result.identity_unknown = true;
                    result
                        .unknown
                        .push(format!("{}: unsupported mapping metadata", attr.location));
                }
                "Meta" if class.orm == ModelOrm::Tortoise => {
                    result.identity_unknown = true;
                    result.eager_unknown = true;
                    result
                        .unknown
                        .push(format!("{}: unresolved Meta metadata", attr.location));
                }
                "__allow_unmapped__" if matches!(attr.value, Expr::Bool(_)) => {}
                "__doc__" if matches!(attr.value, Expr::String(_) | Expr::None) => {}
                name if name.starts_with("__") => {
                    result.identity_unknown = true;
                    result.eager_unknown |= self.dynamic_mapping_value(&expanded, &class.module);
                    result
                        .unknown
                        .push(format!("{}: unsupported mapping hook", attr.location));
                }
                _ => {
                    let mut field = Resolved::default();
                    self.field(class, attr, annotation, expanded, &mut field);
                    result.mapped_attributes.remove(&attr.name);
                    if !field.columns.is_empty()
                        && field.unknown.is_empty()
                        && !field.identity_unknown
                    {
                        result.mapped_attributes.insert(attr.name.clone());
                    }
                    result.columns.extend(field.columns);
                    result.unknown.extend(field.unknown);
                    result.primary_key |= field.primary_key;
                    result.identity_unknown |= field.identity_unknown;
                    result.eager_unknown |= field.eager_unknown;
                }
            }
        }
        if class.orm == ModelOrm::Tortoise {
            if result.schema.is_some() && !class.meta.iter().any(|a| a.name == "schema") {
                result.identity_unknown = true;
                result.unknown.push(format!(
                    "{}: inherited Tortoise schema cannot be resolved",
                    class.location
                ));
            }
            // Concrete Tortoise models derive their own table name when Meta.table is absent.
            if !class.meta.iter().any(|a| a.name == "table") {
                result.table = None;
            }
            // Meta belongs to the concrete class; abstractness is not inherited.
            for attr in &class.meta {
                match attr.name.as_str() {
                    "table" => match &attr.value {
                        Expr::String(value) => result.table = Some(value.clone()),
                        _ => {
                            result.identity_unknown = true;
                            result
                                .unknown
                                .push(format!("{}: dynamic table name", attr.location));
                        }
                    },
                    "schema" => match &attr.value {
                        Expr::String(value) => result.schema = Some(value.clone()),
                        _ => {
                            result.identity_unknown = true;
                            result
                                .unknown
                                .push(format!("{}: dynamic schema", attr.location));
                        }
                    },
                    "abstract" => match attr.value {
                        Expr::Bool(value) => result.abstract_model = value,
                        _ => result
                            .unknown
                            .push(format!("{}: dynamic abstract flag", attr.location)),
                    },
                    _ => {}
                }
            }
            if result.model && result.table.is_none() && !result.abstract_model {
                result.table = Some(class.name.to_lowercase());
            }
            if result.model && !result.abstract_model && !result.primary_key {
                result
                    .columns
                    .entry("id".into())
                    .or_insert_with(|| format!("{} (implicit primary key)", class.location));
            }
        }
        if class.unsupported {
            result.eager_unknown = true;
            result.identity_unknown = true;
            result.unknown.push(format!(
                "{}: unsupported class-body construct or decorator",
                class.location
            ));
        }
        // A plain mixin can carry fields; its unknown descriptors matter when inherited.
        if !result.model && !result.columns.is_empty() {
            result.abstract_model = true;
        }
        visiting.remove(&key);
        if result.unknown.is_empty()
            && result.references.unknown.is_empty()
            && !result.identity_unknown
            && !result.eager_unknown
        {
            let cost = result.copy_cost();
            if cost < self.proof_work.get() {
                self.proof_work.set(self.proof_work.get() - cost);
                self.resolved_classes
                    .borrow_mut()
                    .insert(cache_key, result.clone());
            }
        }
        result
    }
    fn unknown_type(&self, value: &Expr, module: &str, nested: bool) -> bool {
        match value {
            Expr::Name(name) => {
                !matches!(
                    name.as_str(),
                    "int"
                        | "str"
                        | "bytes"
                        | "bool"
                        | "float"
                        | "dict"
                        | "list"
                        | "set"
                        | "tuple"
                        | "object"
                        | "None"
                        | "datetime.datetime"
                        | "datetime.date"
                        | "datetime.time"
                        | "datetime.timedelta"
                        | "uuid.UUID"
                        | "decimal.Decimal"
                        | "typing.Any"
                        | "typing.Self"
                ) && !self.classes.contains_key(name)
            }
            Expr::String(name) => !self.classes.contains_key(
                &self
                    .symbol(&Expr::Name(name.clone()), module, 0)
                    .unwrap_or_default(),
            ),
            Expr::Subscript(inner, args) => {
                let name = self.symbol(inner, module, 0).unwrap_or_default();
                if name == "typing.Literal" {
                    return false;
                }
                if matches!(
                    name.as_str(),
                    "typing.Annotated" | "typing_extensions.Annotated"
                ) {
                    return nested
                        || args
                            .first()
                            .is_none_or(|arg| self.unknown_type(arg, module, true));
                }
                let supported = matches!(
                    name.as_str(),
                    "list"
                        | "dict"
                        | "set"
                        | "tuple"
                        | "typing.Optional"
                        | "typing.Union"
                        | "typing.List"
                        | "typing.Dict"
                        | "typing.Set"
                        | "typing.Tuple"
                ) || self.classes.contains_key(&name);
                !supported || args.iter().any(|arg| self.unknown_type(arg, module, true))
            }
            Expr::Union(args) => args.iter().any(|arg| self.unknown_type(arg, module, true)),
            Expr::Literal | Expr::None => false,
            _ => true,
        }
    }
    fn field(
        &self,
        class: &Class,
        attr: &Attribute,
        annotation: Expr,
        mut value: Expr,
        result: &mut Resolved,
    ) {
        if contains_limit(&annotation) {
            result.identity_unknown = true;
            result.eager_unknown = true;
            result.unknown.push(format!(
                "{}: annotation expansion limit exceeded",
                attr.location
            ));
            return;
        }
        let classvar = match &annotation {
            Expr::Name(name) => name == "typing.ClassVar",
            Expr::Subscript(inner, _) => {
                self.symbol(inner, &class.module, 0).as_deref() == Some("typing.ClassVar")
            }
            _ => false,
        };
        if classvar {
            if !matches!(&annotation, Expr::Name(name) if name == "typing.ClassVar")
                && !self.data_type(&annotation, &mut HashSet::new())
            {
                result.identity_unknown = true;
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: ClassVar annotation is not proven inert",
                    attr.location
                ));
                return;
            }
            if attr.name == "registry" {
                let registry = &value;
                if !self.registry(registry, &class.module) {
                    result.identity_unknown = true;
                    result.eager_unknown = true;
                    result.unknown.push(format!(
                        "{}: unsupported registry may alter table identity",
                        attr.location
                    ));
                }
                return;
            }
            if !matches!(&value, Expr::Call(function, _, _)
                if matches!(self.symbol(function, &class.module, 0).as_deref(), Some("sqlalchemy.orm.mapped_column" | "sqlalchemy.Column" | "sqlalchemy.schema.Column")))
            {
                result.eager_unknown = true;
                result.unknown.push(format!(
                    "{}: class metadata is not proven inert",
                    attr.location
                ));
                return;
            }
        }
        let mut annotated_column = None;
        if class.orm == ModelOrm::Sqlalchemy {
            if let Expr::Subscript(wrapper, args) = &annotation {
                if self.symbol(wrapper, &class.module, 0).as_deref()
                    == Some("sqlalchemy.orm.Mapped")
                {
                    if args.len() != 1 {
                        result
                            .unknown
                            .push(format!("{}: unresolved mapped type alias", attr.location));
                    }
                    for argument in args {
                        let argument = non_nullable_type(argument);
                        if self.unknown_type(argument, &class.module, false) {
                            result
                                .unknown
                                .push(format!("{}: unresolved mapped type alias", attr.location));
                        }
                        if let Expr::Subscript(inner, metadata) = argument {
                            if matches!(
                                self.symbol(inner, &class.module, 0).as_deref(),
                                Some("typing.Annotated" | "typing_extensions.Annotated")
                            ) {
                                for item in metadata.iter().skip(1) {
                                    if matches!(item, Expr::Call(function, _, _) if self.symbol(function, &class.module, 0).as_deref() == Some("sqlalchemy.orm.mapped_column"))
                                        && annotated_column.is_none()
                                    {
                                        annotated_column = Some(item.clone());
                                    } else {
                                        result.unknown.push(format!(
                                            "{}: unresolved Annotated metadata",
                                            attr.location
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(Expr::Call(_, alias_args, alias_kwargs)) = &annotated_column {
            match &mut value {
                Expr::Missing => value = annotated_column.unwrap(),
                Expr::Call(function, args, kwargs)
                    if self.symbol(function, &class.module, 0).as_deref()
                        == Some("sqlalchemy.orm.mapped_column") =>
                {
                    // Explicit options override the Annotated template; unnamed columns inherit its name.
                    if !matches!(args.first(), Some(Expr::String(_))) {
                        if args.first().is_some_and(|first| {
                            let name = match first {
                                Expr::Name(name) => Some(name.as_str()),
                                Expr::Call(function, _, _) => {
                                    if let Expr::Name(name) = &**function {
                                        Some(name.as_str())
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            };
                            !name.is_some_and(|name| name.starts_with("sqlalchemy."))
                        }) {
                            result.unknown.push(format!(
                                "{}: unresolved explicit column name/type",
                                attr.location
                            ));
                        }
                        if let Some(Expr::String(name)) = alias_args.first() {
                            args.insert(0, Expr::String(name.clone()));
                        }
                    }
                    for (key, option) in alias_kwargs {
                        if key == "name" && matches!(args.first(), Some(Expr::String(_))) {
                            continue;
                        }
                        kwargs.entry(key.clone()).or_insert_with(|| option.clone());
                    }
                }
                _ => result.unknown.push(format!(
                    "{}: unresolved explicit Annotated column value",
                    attr.location
                )),
            }
        }
        if contains_limit(&value) {
            result.identity_unknown = true;
            result.eager_unknown = true;
            result
                .unknown
                .push(format!("{}: field expansion limit exceeded", attr.location));
            return;
        }
        let mut column_call = None;
        if let Expr::Call(function, args, kwargs) = &value {
            let name = self.symbol(function, &class.module, 0).unwrap_or_default();
            match class.orm {
                ModelOrm::Sqlalchemy => {
                    if matches!(
                        name.as_str(),
                        "sqlalchemy.Column"
                            | "sqlalchemy.orm.mapped_column"
                            | "sqlalchemy.schema.Column"
                    ) {
                        column_call = Some((args, kwargs, false));
                    } else if matches!(
                        name.as_str(),
                        "sqlalchemy.orm.relationship"
                            | "sqlalchemy.orm.column_property"
                            | "sqlalchemy.orm.synonym"
                    ) {
                        if name == "sqlalchemy.orm.relationship" && kwargs.contains_key("secondary")
                        {
                            result.identity_unknown = true;
                            result.unknown.push(format!(
                                "{}: secondary relationship table cannot be resolved",
                                attr.location
                            ));
                        }
                        if name != "sqlalchemy.orm.relationship" {
                            result.unknown.push(format!(
                                "{}: descriptor can refer to a column",
                                attr.location
                            ));
                        }
                        return;
                    } else {
                        result.identity_unknown = true;
                        result.eager_unknown = true;
                        result.unknown.push(format!(
                            "{}: unsupported field expression {name}",
                            attr.location
                        ));
                        return;
                    }
                }
                ModelOrm::Tortoise => {
                    if name.starts_with("tortoise.fields.") {
                        let field = name.rsplit('.').next().unwrap_or("");
                        if matches!(
                            field,
                            "ManyToManyField" | "BackwardFKRelation" | "BackwardOneToOneRelation"
                        ) {
                            if field == "ManyToManyField" {
                                result.identity_unknown = true;
                                result.unknown.push(format!(
                                    "{}: unresolved many-to-many through table",
                                    attr.location
                                ));
                            }
                            return;
                        }
                        if TORTOISE_FIELDS.contains(&field) {
                            column_call = Some((
                                args,
                                kwargs,
                                matches!(field, "ForeignKeyField" | "OneToOneField"),
                            ));
                        } else {
                            result.eager_unknown = true;
                            result.unknown.push(format!(
                                "{}: unsupported Tortoise field {field}",
                                attr.location
                            ));
                            return;
                        }
                    } else {
                        result.identity_unknown = true;
                        result.eager_unknown = true;
                        result
                            .unknown
                            .push(format!("{}: unresolved custom field", attr.location));
                        return;
                    }
                }
            }
        }
        if let Some((args, kwargs, relation)) = column_call {
            let mut name = if relation {
                format!("{}_id", attr.name)
            } else {
                attr.name.clone()
            };
            if class.orm == ModelOrm::Sqlalchemy {
                if let Some(Expr::String(explicit)) = args.first() {
                    name = explicit.clone();
                } else if let Some(first) = args.first() {
                    let type_name = match first {
                        Expr::Name(name) => Some(name.as_str()),
                        Expr::Call(function, _, _) => {
                            if let Expr::Name(name) = &**function {
                                Some(name.as_str())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if !type_name.is_some_and(|name| name.starts_with("sqlalchemy.")) {
                        result
                            .unknown
                            .push(format!("{}: unresolved column name/type", attr.location));
                    }
                }
            }
            let name_key = if class.orm == ModelOrm::Sqlalchemy {
                "name"
            } else {
                "source_field"
            };
            if let Some(explicit) = kwargs.get(name_key) {
                match explicit {
                    Expr::String(explicit) => name = explicit.clone(),
                    _ => result
                        .unknown
                        .push(format!("{}: dynamic database column name", attr.location)),
                }
            }
            if matches!(
                kwargs.get("primary_key").or_else(|| kwargs.get("pk")),
                Some(Expr::Bool(true))
            ) {
                result.primary_key = true;
            }
            if kwargs.contains_key("**") {
                result
                    .unknown
                    .push(format!("{}: dynamic column options", attr.location));
            }
            if name.is_empty() {
                result
                    .unknown
                    .push(format!("{}: empty database column name", attr.location));
            }
            result.columns.insert(name, attr.location.clone());
            return;
        }
        if class.orm == ModelOrm::Sqlalchemy {
            if let Expr::Subscript(wrapper, _) = &annotation {
                let name = self.symbol(wrapper, &class.module, 0).unwrap_or_default();
                if name == "sqlalchemy.orm.Mapped" {
                    result
                        .columns
                        .insert(attr.name.clone(), attr.location.clone());
                    return;
                }
                if name == "typing.ClassVar" {
                    return;
                }
            }
        }
        // Literals are plain application constants; unknown descriptors are not.
        result.eager_unknown |= self.dynamic_mapping_value(&value, &class.module);
        if !matches!(
            value,
            Expr::String(_) | Expr::Bool(_) | Expr::Literal | Expr::None
        ) {
            result
                .unknown
                .push(format!("{}: unresolved field annotation", attr.location));
        }
    }
}
fn attrs_fn(body: Node<'_>, source: &str, path: &std::path::Path) -> (Vec<Attribute>, bool) {
    attrs(body, source, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn index(orm: ModelOrm, source: &str) -> ModelIndex {
        ModelIndex::build(vec![ModelFile {
            path: "app/models.py".into(),
            orm,
            source: source.into(),
        }])
    }
    fn table(name: &str) -> TableIdentity {
        TableIdentity {
            name: name.into(),
            schema: None,
        }
    }
    #[test]
    fn gateway_registry_annotated_alias_and_constraint_fixture() {
        let shared = "from typing import Annotated, ClassVar\nfrom sqlalchemy import BigInteger, DateTime, Identity\nfrom sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, registry\nsql_orm_registry = registry(type_annotation_map={\n# Type defaults only.\ndict: DateTime(timezone=True)})\nBigIntPk = Annotated[int, mapped_column(BigInteger, Identity(), primary_key=True)]\nclass Base(DeclarativeBase):\n    registry: ClassVar = sql_orm_registry\nclass TimestampMixin:\n    created_at: Mapped[int] = mapped_column()\n";
        let concrete = "from sqlalchemy import UniqueConstraint, CheckConstraint\nfrom sqlalchemy.orm import Mapped\nfrom .base import Base, BigIntPk, TimestampMixin\nclass Provenance(TimestampMixin, Base):\n    __tablename__ = 'jobs'\n    __table_args__ = (UniqueConstraint('id'), CheckConstraint('id > 0'))\n    id: Mapped[BigIntPk]\n";
        let index = ModelIndex::build(vec![
            ModelFile {
                path: "app/base.py".into(),
                orm: ModelOrm::Sqlalchemy,
                source: shared.into(),
            },
            ModelFile {
                path: "app/job.py".into(),
                orm: ModelOrm::Sqlalchemy,
                source: concrete.into(),
            },
        ]);
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "retired")
            .is_ok());
        for field in ["id", "created_at"] {
            assert!(index
                .column_absence(MigrationFramework::Alembic, &table("jobs"), field)
                .is_err());
        }
    }
    #[test]
    fn nullable_imported_annotated_aliases_resolve_names() {
        for annotation in [
            "Alias",
            "Alias | None",
            "None | Alias",
            "Optional[Alias]",
            "Union[Alias, None]",
            "shared.Alias | None",
        ] {
            for assignment in [
                "",
                " = mapped_column()",
                " = mapped_column(name='override')",
            ] {
                let sources = vec![
                    ModelFile { path: "app/shared.py".into(), orm: ModelOrm::Sqlalchemy,
                        source: "from typing import Annotated\nfrom sqlalchemy import BigInteger\nfrom sqlalchemy.orm import DeclarativeBase, mapped_column\nAlias = Annotated[int, mapped_column('signed_manifest', BigInteger)]\nclass Base(DeclarativeBase):\n    pass\n".into() },
                    ModelFile { path: "app/models.py".into(), orm: ModelOrm::Sqlalchemy,
                        source: format!("from typing import Optional, Union\nfrom sqlalchemy.orm import Mapped, mapped_column\nfrom .shared import Base, Alias\nfrom . import shared\nclass Job(Base):\n    __tablename__ = 'jobs'\n    manifest: Mapped[{annotation}]{assignment}\n") },
                ];
                let index = ModelIndex::build(sources);
                assert!(
                    index
                        .column_absence(MigrationFramework::Alembic, &table("jobs"), "retired")
                        .is_ok(),
                    "{annotation}{assignment}"
                );
                let name = if assignment.contains("override") {
                    "override"
                } else {
                    "signed_manifest"
                };
                let error = index
                    .column_absence(MigrationFramework::Alembic, &table("jobs"), name)
                    .unwrap_err();
                assert!(
                    error.contains("still mapped"),
                    "{annotation}{assignment}: {error}"
                );
                if name == "override" {
                    assert!(index
                        .column_absence(
                            MigrationFramework::Alembic,
                            &table("jobs"),
                            "signed_manifest"
                        )
                        .is_ok());
                }
            }
        }
    }

    #[test]
    fn nullable_annotated_unknown_and_ambiguous_forms_fail_closed() {
        for expression in [
            "Alias | str",
            "list[Alias]",
            "Missing | None",
            "Cycle | None",
            "Annotated[Alias, mapped_column('other')] | None",
        ] {
            let source = format!("from typing import Annotated\nfrom sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\nAlias = Annotated[bytes, mapped_column('signed_manifest')]\nCycle = Cycle\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    value: Mapped[{expression}]\n");
            assert!(
                index(ModelOrm::Sqlalchemy, &source)
                    .column_absence(
                        MigrationFramework::Alembic,
                        &table("jobs"),
                        "signed_manifest"
                    )
                    .is_err(),
                "{expression}"
            );
        }
    }

    #[test]
    fn annotated_alias_cannot_hide_dynamic_explicit_column_values() {
        for assignment in [
            "mapped_column(choose_name())",
            "mapped_column(unknown_name)",
            "unknown_descriptor",
            "something if flag else other",
            "mapped_column(name=choose_name())",
        ] {
            let source = format!("from typing import Annotated\nfrom sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\nAlias = Annotated[bytes, mapped_column('template_name')]\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    manifest: Mapped[Alias | None] = {assignment}\n");
            assert!(
                index(ModelOrm::Sqlalchemy, &source)
                    .column_absence(
                        MigrationFramework::Alembic,
                        &table("jobs"),
                        "signed_manifest"
                    )
                    .is_err(),
                "{assignment}"
            );
        }
    }

    #[test]
    fn explicit_positional_name_overrides_keyword_template_name() {
        let source = "from typing import Annotated\nfrom sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\nAlias = Annotated[bytes, mapped_column(name='old')]\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    manifest: Mapped[Alias | None] = mapped_column('signed_manifest')\n";
        let index = index(ModelOrm::Sqlalchemy, source);
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "old")
            .is_ok());
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .unwrap_err()
            .contains("still mapped"));
    }

    #[test]
    fn gateway_module_constants_are_static_in_either_file_order() {
        let shared = "PREFIX = 'ai_tool_bundle'\nSUFFIX_LENGTH = 21\n";
        let concrete = "import string\nfrom .constants import PREFIX, SUFFIX_LENGTH\nfrom sqlalchemy.orm import DeclarativeBase, Mapped\nLENGTH = len(PREFIX) + 1 + SUFFIX_LENGTH\n_BASE62 = string.ascii_letters + string.digits\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    id: Mapped[int]\n";
        for reverse in [false, true] {
            let mut files = vec![
                ModelFile {
                    path: "app/constants.py".into(),
                    orm: ModelOrm::Sqlalchemy,
                    source: shared.into(),
                },
                ModelFile {
                    path: "app/models.py".into(),
                    orm: ModelOrm::Sqlalchemy,
                    source: concrete.into(),
                },
            ];
            if reverse {
                files.reverse();
            }
            let index = ModelIndex::build(files);
            assert!(index
                .column_absence(
                    MigrationFramework::Alembic,
                    &table("jobs"),
                    "signed_manifest"
                )
                .is_ok());
        }
    }

    #[test]
    fn scalar_lookalikes_and_side_effects_keep_coverage_unknown() {
        for expression in [
            "len(register_models()) + 1",
            "len(unknown) + 1",
            "custom + 1",
            "string.unknown + string.digits",
            "len = register_models\nLENGTH = len('prefix')",
            "len = len\nLENGTH = len('prefix')",
        ] {
            let source = format!("import string\nfrom sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy import String\nLENGTH = {expression}\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    value = mapped_column(String(LENGTH))\n");
            assert!(
                index(ModelOrm::Sqlalchemy, &source)
                    .column_absence(
                        MigrationFramework::Alembic,
                        &table("jobs"),
                        "signed_manifest"
                    )
                    .is_err(),
                "{expression}"
            );
        }
    }

    #[test]
    fn annotated_metadata_name_is_present_even_with_explicit_column() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from typing import Annotated\nfrom sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\nAlias = Annotated[bytes, mapped_column('signed_manifest')]\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    manifest: Mapped[Alias] = mapped_column()\n",
        );
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn dynamic_model_factories_and_mapping_hooks_fail_closed() {
        let base = "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n";
        for dynamic in [
            "Other = type('Other', (Base,), {})\n",
            "while ready():\n    register_models()\n",
            "class Other(Base):\n    __annotations__ = {'signed_manifest': Mapped[bytes]}\n",
            "class Hook(Base):\n    def __init_subclass__(cls):\n        cls.signed_manifest = field()\n",
        ] {
            assert!(
                index(ModelOrm::Sqlalchemy, &format!("{base}{dynamic}"))
                    .column_absence(
                        MigrationFramework::Alembic,
                        &table("jobs"),
                        "signed_manifest"
                    )
                    .is_err(),
                "{dynamic}"
            );
        }
    }
    #[test]
    fn tortoise_dynamic_or_inherited_meta_poison_matching_candidates() {
        let base = "from tortoise.models import Model\nclass Job(Model):\n    class Meta:\n        table = 'jobs'\n";
        for meta in [
            "    class Meta(CustomMeta):\n        pass\n",
            "    class Meta:\n        table = choose_table()\n",
            "    class Meta:\n        if ready():\n            table = 'jobs'\n",
        ] {
            assert!(
                index(
                    ModelOrm::Tortoise,
                    &format!("{base}class Other(Model):\n{meta}")
                )
                .column_absence(
                    MigrationFramework::Aerich,
                    &table("jobs"),
                    "signed_manifest"
                )
                .is_err(),
                "{meta}"
            );
        }
    }
    #[test]
    fn explicit_column_descriptors_override_classvar() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from typing import ClassVar\nfrom sqlalchemy import LargeBinary\nfrom sqlalchemy.orm import DeclarativeBase, mapped_column\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    manifest: ClassVar[bytes] = mapped_column('signed_manifest', LargeBinary)\n",
        );
        let present = index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest",
            )
            .unwrap_err();
        assert!(present.contains("still mapped"), "{present}");
    }
    #[test]
    fn shadowed_constructor_and_module_decorator_fail_closed() {
        let base = "from sqlalchemy.orm import DeclarativeBase, mapped_column\nclass Base(DeclarativeBase):\n    pass\n";
        {
            let dynamic = "def mapped_column(*args):\n    return renamed_column(*args)\n";
            let source = format!(
                "{base}{dynamic}class Job(Base):\n    __tablename__ = 'jobs'\n    manifest = mapped_column()\n"
            );
            assert!(index(ModelOrm::Sqlalchemy, &source)
                .column_absence(
                    MigrationFramework::Alembic,
                    &table("jobs"),
                    "signed_manifest"
                )
                .is_err());
        }
    }
    #[test]
    fn branching_inheritance_is_bounded() {
        let mut source = String::from(
            "from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\n",
        );
        for n in 0..22 {
            let parents = if n == 0 {
                "Base".to_owned()
            } else {
                format!("Left{}, Right{}", n - 1, n - 1)
            };
            source.push_str(&format!("class Left{n}({parents}):\n    __abstract__ = True\nclass Right{n}({parents}):\n    __abstract__ = True\n"));
        }
        source.push_str("class Job(Left21, Right21):\n    __tablename__ = 'jobs'\n");
        assert!(index(ModelOrm::Sqlalchemy, &source)
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn unspecified_schema_consumers_overlap_explicit_schema_targets() {
        for (clean_schema, used_schema) in [
            ("    __table_args__ = {'schema': 'public'}\n", ""),
            ("", "    __table_args__ = {'schema': 'public'}\n"),
        ] {
            let source = format!(
                "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\nclass Clean(Base):\n    __tablename__ = 'jobs'\n{clean_schema}    id: Mapped[int]\nclass Used(Base):\n    __tablename__ = 'jobs'\n{used_schema}    signed_manifest: Mapped[bytes]\n"
            );
            let index = index(ModelOrm::Sqlalchemy, &source);
            for schema in [Some("public".into()), None] {
                assert!(index
                    .column_absence(
                        MigrationFramework::Alembic,
                        &TableIdentity {
                            name: "jobs".into(),
                            schema
                        },
                        "signed_manifest"
                    )
                    .is_err());
            }
        }
    }
    #[test]
    fn own_table_options_replace_inherited_schema() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\nclass SchemaMixin:\n    __table_args__ = {'schema': 'archive'}\nclass Clean(Base):\n    __tablename__ = 'jobs'\n    id: Mapped[int]\nclass Used(SchemaMixin, Base):\n    __tablename__ = 'jobs'\n    __table_args__ = {'extend_existing': True}\n    signed_manifest: Mapped[bytes]\n",
        );
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .unwrap_err()
            .contains("still mapped"));
    }
    #[test]
    fn abstract_models_never_establish_concrete_table_coverage() {
        let sqlalchemy = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\nclass Template(Base):\n    __abstract__ = True\n    __tablename__ = 'jobs'\n",
        );
        assert!(sqlalchemy
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "retired")
            .is_err());
        let tortoise = index(
            ModelOrm::Tortoise,
            "from tortoise.models import Model\nclass Template(Model):\n    class Meta:\n        abstract = True\n        table = 'jobs'\n",
        );
        assert!(tortoise
            .column_absence(MigrationFramework::Aerich, &table("jobs"), "retired")
            .is_err());
    }
    #[test]
    fn table_metadata_without_orm_ancestry_is_not_evidence() {
        for (orm, framework, source) in [
            (
                ModelOrm::Sqlalchemy,
                MigrationFramework::Alembic,
                "class Job:\n    __tablename__ = 'jobs'\n",
            ),
            (
                ModelOrm::Tortoise,
                MigrationFramework::Aerich,
                "class Job:\n    class Meta:\n        table = 'jobs'\n",
            ),
        ] {
            assert!(index(orm, source)
                .column_absence(framework, &table("jobs"), "retired")
                .is_err());
        }
    }
    #[test]
    fn unsupported_explicit_and_generated_tables_poison_other_candidates() {
        let sqlalchemy = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy import Table, Column, LargeBinary\nfrom sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\nclass Clean(Base):\n    __tablename__ = 'jobs'\nclass Other(Base):\n    __tablename__ = 'others'\n    __table__ = Table('jobs', Base.metadata, Column('signed_manifest', LargeBinary))\n",
        );
        assert!(sqlalchemy
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
        let tortoise = index(
            ModelOrm::Tortoise,
            "from tortoise import fields\nfrom tortoise.models import Model\nclass Clean(Model):\n    class Meta:\n        table = 'job_tag'\nclass Other(Model):\n    tags = fields.ManyToManyField('models.Tag', through='job_tag', forward_key='signed_manifest')\n",
        );
        assert!(tortoise
            .column_absence(
                MigrationFramework::Aerich,
                &table("job_tag"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn mixed_providers_with_overlapping_schema_must_agree() {
        let index = ModelIndex::build(vec![ModelFile {path:"sa.py".into(),orm:ModelOrm::Sqlalchemy,source:"from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\nclass Clean(Base):\n    __tablename__ = 'jobs'\n    __table_args__ = {'schema': 'public'}\n".into()}, ModelFile{path:"to.py".into(),orm:ModelOrm::Tortoise,source:"from tortoise import fields\nfrom tortoise.models import Model\nclass Used(Model):\n    signed_manifest = fields.BinaryField()\n    class Meta:\n        table = 'jobs'\n".into()}]);
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &TableIdentity {
                    name: "jobs".into(),
                    schema: Some("public".into())
                },
                "signed_manifest"
            )
            .unwrap_err()
            .contains("still mapped"));
    }
    #[test]
    fn nested_class_models_cannot_hide_usage() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\nclass Clean(Base):\n    __tablename__ = 'jobs'\nclass Container:\n    class Legacy(Base):\n        __tablename__ = 'jobs'\n        signed_manifest: Mapped[bytes]\n",
        );
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn registry_constructor_cannot_hide_side_effects() {
        for registry in [
            "registry(type_annotation_map=create_models())",
            "registry(type_annotation_map={dict: create_models()})",
        ] {
            let source = format!(
                "from typing import ClassVar\nfrom sqlalchemy.orm import DeclarativeBase, registry\nclass Base(DeclarativeBase):\n    registry: ClassVar = {registry}\nclass Clean(Base):\n    __tablename__ = 'jobs'\n"
            );
            assert!(index(ModelOrm::Sqlalchemy, &source)
                .column_absence(
                    MigrationFramework::Alembic,
                    &table("jobs"),
                    "signed_manifest"
                )
                .is_err());
        }
    }
    #[test]
    fn plain_mixin_class_construction_hooks_poison_descendants() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\nclass HookMixin:\n    def __init_subclass__(cls):\n        register_signed_manifest(cls)\nclass Job(HookMixin, Base):\n    __tablename__ = 'jobs'\n",
        );
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn unknown_external_annotated_alias_cannot_establish_absence() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from missing import SecretAlias\nfrom sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    manifest: Mapped[SecretAlias]\n",
        );
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn alias_expansion_work_limit_is_fail_closed() {
        let mut source =
            String::from("from sqlalchemy.orm import DeclarativeBase, Mapped\nA0 = (1,1)\n");
        for n in 1..18 {
            source.push_str(&format!("A{n} = (A{}, A{})\n", n - 1, n - 1));
        }
        source.push_str("class Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    manifest: Mapped[A17]\n");
        assert!(index(ModelOrm::Sqlalchemy, &source)
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn overlapping_orm_sources_cannot_silently_discard_one_interpretation() {
        let files = [ModelOrm::Sqlalchemy, ModelOrm::Tortoise]
            .into_iter()
            .map(|orm| ModelFile {
                path: "models.py".into(),
                orm,
                source: "class Plain: pass\n".into(),
            })
            .collect();
        let reason = ModelIndex::build(files)
            .table_absence(&table("retired"))
            .unwrap_err();
        assert!(
            reason.contains("ambiguous module/package source"),
            "{reason}"
        );
    }
    #[test]
    fn cached_ancestry_preserves_depth_and_copy_bounds() {
        let mut source = String::from(
            "from sqlalchemy.orm import DeclarativeBase\nclass C0(DeclarativeBase):\n    pass\n",
        );
        for n in 1..=128 {
            source.push_str(&format!("class C{n}(C{}):\n    pass\n", n - 1));
        }
        for warm in [false, true] {
            let mut sources = Sources {
                proof_work: Cell::new(4096),
                ..Sources::default()
            };
            sources.read(
                ModelFile {
                    path: "app/models.py".into(),
                    orm: ModelOrm::Sqlalchemy,
                    source: source.clone(),
                },
                None,
            );
            sources.cache_symbols = true;
            if warm {
                let valid = sources.resolve_class(
                    &sources.classes["app.models.C127"],
                    &mut HashSet::new(),
                    true,
                );
                assert!(valid.unknown.is_empty(), "{:?}", valid.unknown);
            }
            let invalid = sources.resolve_class(
                &sources.classes["app.models.C128"],
                &mut HashSet::new(),
                true,
            );
            assert!(
                invalid
                    .unknown
                    .iter()
                    .any(|reason| reason.contains("cyclic inheritance")),
                "{:?}",
                invalid.unknown
            );
            assert!(sources.proof_work.get() > 0);
            if !warm {
                let valid = sources.resolve_class(
                    &sources.classes["app.models.C127"],
                    &mut HashSet::new(),
                    true,
                );
                assert!(valid.unknown.is_empty(), "{:?}", valid.unknown);
            }
        }
        let mut sources = Sources {
            proof_work: Cell::new(4096),
            ..Sources::default()
        };
        sources.read(ModelFile { path: "app/models.py".into(), orm: ModelOrm::Sqlalchemy, source: "from sqlalchemy.orm import DeclarativeBase, mapped_column\nclass Wide(DeclarativeBase):\n    __tablename__ = 'wide'\n".to_owned() + &(0..64).map(|n| format!("    value{n} = mapped_column()\n")).collect::<String>() }, None);
        sources.cache_symbols = true;
        let class = &sources.classes["app.models.Wide"];
        assert!(sources
            .resolve_class(class, &mut HashSet::new(), true)
            .unknown
            .is_empty());
        let result = (0..32)
            .find_map(|_| {
                let result = sources.resolve_class(class, &mut HashSet::new(), true);
                (!result.unknown.is_empty()).then_some(result)
            })
            .unwrap();
        assert!(result
            .unknown
            .iter()
            .any(|reason| reason.contains("work limit exceeded")));
        assert_eq!(sources.proof_work.get(), 0);
    }

    #[test]
    fn cached_record_shapes_preserve_depth_and_argument_checks() {
        let mut source =
            String::from("from typing import NamedTuple\nclass R0(NamedTuple):\n    text: str\n");
        for n in 1..=32 {
            source.push_str(&format!("class R{n}(NamedTuple):\n    child: R{}\n", n - 1));
        }
        for warm in [false, true] {
            let mut sources = Sources {
                proof_work: Cell::new(4096),
                ..Sources::default()
            };
            sources.read(
                ModelFile {
                    path: "app/records.py".into(),
                    orm: ModelOrm::Sqlalchemy,
                    source: source.clone(),
                },
                None,
            );
            sources.cache_symbols = true;
            if warm {
                assert!(
                    sources.data_class(&sources.classes["app.records.R31"], &mut HashSet::new())
                );
            }
            assert!(!sources.data_class(&sources.classes["app.records.R32"], &mut HashSet::new()));
            assert!(sources.proof_work.get() > 0);
            assert!(sources.data_class(&sources.classes["app.records.R0"], &mut HashSet::new()));
            assert!(!sources.inert_value(
                &Expr::Call(
                    Box::new(Expr::Name("app.records.R0".into())),
                    vec![Expr::Other],
                    HashMap::new()
                ),
                "",
                &mut HashSet::new()
            ));
        }
    }

    #[test]
    fn independent_declarations_share_the_resolution_budget() {
        let mut source = String::from("from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\n");
        for n in 0..4097 {
            source.push_str(&format!(
                "class Item{n}(Base):\n    __tablename__ = 'item{n}'\n    id: Mapped[int]\n"
            ));
        }
        let reason = index(ModelOrm::Sqlalchemy, &source)
            .table_absence(&table("retired"))
            .unwrap_err();
        assert!(reason.contains("work limit exceeded"), "{reason}");
    }
    #[test]
    fn ordinary_builtin_optional_annotations_are_supported() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    value: Mapped[str | None]\n",
        );
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "retired")
            .is_ok());
    }
    #[test]
    fn sqlalchemy_bare_mapped_and_database_alias() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\nfrom sqlalchemy import LargeBinary\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\n    id: Mapped[int]\n    manifest = mapped_column('signed_manifest', LargeBinary)\n",
        );
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "retired")
            .is_ok());
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "id")
            .is_err());
        assert!(index
            .column_absence(
                MigrationFramework::Alembic,
                &table("jobs"),
                "signed_manifest"
            )
            .is_err());
    }
    #[test]
    fn imported_base_and_inherited_mixin() {
        let files = vec![ModelFile { path: "app/base.py".into(), orm: ModelOrm::Sqlalchemy, source: "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Base(DeclarativeBase):\n    pass\nclass Mixin:\n    retained: Mapped[int]\n".into() }, ModelFile { path: "app/jobs.py".into(), orm: ModelOrm::Sqlalchemy, source: "from .base import Base, Mixin\nclass Job(Mixin, Base):\n    __tablename__ = 'jobs'\n".into() }];
        let index = ModelIndex::build(files);
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "retained")
            .is_err());
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "removed")
            .is_ok());
    }
    #[test]
    fn unknown_table_candidate_poison_coverage() {
        let index = index(
            ModelOrm::Sqlalchemy,
            "from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\nclass Job(Base):\n    __tablename__ = 'jobs'\nclass Dynamic(Base):\n    __tablename__ = choose_table()\n",
        );
        assert!(index
            .column_absence(MigrationFramework::Alembic, &table("jobs"), "removed")
            .is_err());
    }
    #[test]
    fn tortoise_source_field_relations_and_implicit_id() {
        let index = index(
            ModelOrm::Tortoise,
            "from tortoise import fields\nfrom tortoise.models import Model\nclass Job(Model):\n    manifest = fields.BinaryField(source_field='signed_manifest')\n    owner = fields.ForeignKeyField('models.User')\n    class Meta:\n        table = 'jobs'\n",
        );
        assert!(index
            .column_absence(MigrationFramework::Aerich, &table("jobs"), "removed")
            .is_ok());
        for field in ["signed_manifest", "owner_id", "id"] {
            assert!(
                index
                    .column_absence(MigrationFramework::Aerich, &table("jobs"), field)
                    .is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn unsupported_and_missing_models_fail_closed() {
        for source in [
            "not valid Python !",
            "class Job(CustomBase):\n    __tablename__ = 'jobs'\n",
            "# no models\n",
        ] {
            assert!(index(ModelOrm::Sqlalchemy, source)
                .column_absence(MigrationFramework::Alembic, &table("jobs"), "removed")
                .is_err());
        }
    }
}
