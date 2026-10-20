//! Conservative, non-executing ORM mapping analysis of immutable Python sources.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tree_sitter::Node;

use crate::ast::TableIdentity;
use crate::config::ModelOrm;
use crate::discovery::MigrationFramework;

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
struct Mapping {
    orm: ModelOrm,
    table: TableIdentity,
    columns: HashMap<String, String>,
    unknown: Vec<String>,
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
    Limit,
    Tuple(Vec<Expr>),
    Union(Vec<Expr>),
    Other,
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
    location: String,
}
#[derive(Default)]
struct Sources {
    classes: HashMap<String, Class>,
    aliases: HashMap<String, (Expr, String)>,
    unknown: Vec<String>,
}
#[derive(Default, Clone)]
struct Resolved {
    model: bool,
    table: Option<String>,
    schema: Option<String>,
    columns: HashMap<String, String>,
    unknown: Vec<String>,
    abstract_model: bool,
    primary_key: bool,
    identity_unknown: bool,
}

impl ModelIndex {
    pub fn build(files: Vec<ModelFile>) -> Self {
        let mut sources = Sources::default();
        for file in files {
            sources.read(file);
        }
        let mut index = Self {
            mappings: vec![],
            unknown: sources.unknown.clone(),
        };
        for class in sources.classes.values() {
            let resolved = sources.resolve_class(class, &mut HashSet::new(), &mut 4096);
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
                    "{}: model table identity is unresolved",
                    class.location
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
                });
            } else if !resolved.abstract_model || !resolved.unknown.is_empty() {
                index.unknown.push(format!(
                    "{}: model table cannot be resolved ({})",
                    class.location,
                    describe(&resolved.unknown)
                ));
            }
        }
        index
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
        Expr::Tuple(values) | Expr::Union(values) => values.iter().any(contains_limit),
        _ => false,
    }
}

fn describe(reasons: &[String]) -> String {
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
        "identifier" | "attribute" => Expr::Name(text(node, source).to_owned()),
        "string" => {
            let raw = text(node, source);
            if raw.len() >= 2
                && (raw.starts_with('\'') || raw.starts_with('"'))
                && !raw.contains('\\')
                && !raw.starts_with("\"\"\"")
                && !raw.starts_with("'''")
            {
                Expr::String(raw[1..raw.len() - 1].into())
            } else {
                Expr::Other
            }
        }
        "integer" | "float" | "none" => Expr::Literal,
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
fn attrs(body: Node<'_>, source: &str, path: &std::path::Path) -> (Vec<Attribute>, bool) {
    let mut result = vec![];
    let mut unsupported = false;
    for statement in named(body) {
        if statement.kind() == "comment" || statement.kind() == "pass_statement" {
            continue;
        }
        if statement.kind() == "expression_statement" {
            let children = named(statement);
            if children.first().is_some_and(|n| n.kind() == "string") {
                continue;
            }
            if let Some(assignment) = children.first().filter(|n| n.kind() == "assignment") {
                if let Some(left) = assignment.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        result.push(Attribute {
                            name: text(left, source).into(),
                            value: assignment
                                .child_by_field_name("right")
                                .map_or(Expr::Other, |n| expr(n, source)),
                            annotation: assignment
                                .child_by_field_name("type")
                                .map_or(Expr::Other, |n| expr(n, source)),
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

impl Sources {
    fn read(&mut self, file: ModelFile) {
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
        let module = module(&file.path);
        let mut pending = vec![(parsed.root_node(), false)];
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
                "import_from_statement" | "import_statement" => self.import(
                    node,
                    &file.source,
                    &module,
                    file.path.file_name().is_some_and(|n| n == "__init__.py"),
                ),
                "class_definition" => self.read_class(node, &file, &module, false),
                "decorated_definition" => {
                    if let Some(class) = named(node)
                        .into_iter()
                        .find(|n| n.kind() == "class_definition")
                    {
                        self.unknown.push(format!(
                            "{}:{}: unsupported class decorator may register mappings",
                            file.path.display(),
                            node.start_position().row + 1
                        ));
                        self.read_class(class, &file, &module, true);
                    } else {
                        for decorator in named(node).into_iter().filter(|n| n.kind() == "decorator")
                        {
                            let expression = named(decorator)
                                .first()
                                .map_or(Expr::Other, |n| expr(*n, &file.source));
                            if !self.safe_decorator(&expression, &module) {
                                self.unknown.push(format!(
                                    "{}:{}: unsupported module decorator may mutate mappings",
                                    file.path.display(),
                                    node.start_position().row + 1
                                ));
                            }
                        }
                        if let Some(function) = named(node)
                            .into_iter()
                            .find(|n| n.kind() == "function_definition")
                        {
                            if let Some(name) = function.child_by_field_name("name") {
                                let key = format!("{module}.{}", text(name, &file.source));
                                if self.aliases.contains_key(&key)
                                    || self.classes.contains_key(&key)
                                {
                                    self.unknown.push(format!(
                                        "{}: symbol {key} rebound",
                                        file.path.display()
                                    ));
                                }
                                self.aliases.insert(key, (Expr::Other, module.clone()));
                            }
                        }
                    }
                }
                "expression_statement" => {
                    if let Some(assignment) =
                        named(node).first().filter(|n| n.kind() == "assignment")
                    {
                        if let (Some(left), Some(right)) = (
                            assignment.child_by_field_name("left"),
                            assignment.child_by_field_name("right"),
                        ) {
                            if left.kind() == "identifier" {
                                let value = expr(right, &file.source);
                                if self.dynamic_global(&value, &module) {
                                    self.unknown.push(format!(
                                        "{}:{}: dynamic module expression may construct mappings",
                                        file.path.display(),
                                        node.start_position().row + 1
                                    ));
                                }
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
                                    self.unknown.push(format!(
                                        "{}: symbol {key} rebound",
                                        file.path.display()
                                    ));
                                }
                                self.aliases.insert(key, (value, module.clone()));
                            } else {
                                self.unknown.push(format!(
                                    "{}:{}: dynamic attribute assignment",
                                    file.path.display(),
                                    node.start_position().row + 1
                                ));
                            }
                        }
                    } else if named(node).first().is_none_or(|n| n.kind() != "string") {
                        self.unknown.push(format!(
                            "{}:{}: top-level call may mutate mappings",
                            file.path.display(),
                            node.start_position().row + 1
                        ));
                    }
                }
                "if_statement" | "for_statement" | "try_statement" | "with_statement"
                | "while_statement" | "match_statement" => {
                    self.unknown.push(format!(
                        "{}:{}: conditional/dynamic module definitions",
                        file.path.display(),
                        node.start_position().row + 1
                    ));
                }
                "function_definition" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let key = format!("{module}.{}", text(name, &file.source));
                        if self.aliases.contains_key(&key) || self.classes.contains_key(&key) {
                            self.unknown
                                .push(format!("{}: symbol {key} rebound", file.path.display()));
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
    }
    fn safe_decorator(&self, expression: &Expr, module: &str) -> bool {
        match expression {
            Expr::Name(_) => matches!(
                self.symbol(expression, module, 0).as_deref(),
                Some(
                    "staticmethod"
                        | "classmethod"
                        | "contextlib.contextmanager"
                        | "contextlib.asynccontextmanager"
                        | "functools.cache"
                        | "typing.overload"
                )
            ),
            Expr::Call(function, args, kwargs) => {
                let name = self.symbol(function, module, 0).unwrap_or_default();
                // This Session event runs on queries, not during mapping construction.
                if name == "sqlalchemy.event.listens_for" && args.len() == 2 && kwargs.is_empty() {
                    return self.symbol(&args[0], module, 0).as_deref()
                        == Some("sqlalchemy.orm.Session")
                        && matches!(&args[1], Expr::String(event) if event == "do_orm_execute");
                }
                name == "functools.lru_cache"
                    && args.iter().all(|arg| matches!(arg, Expr::Literal))
                    && kwargs
                        .values()
                        .all(|arg| matches!(arg, Expr::Literal | Expr::Bool(_)))
            }
            _ => false,
        }
    }
    fn dynamic_global(&self, value: &Expr, module: &str) -> bool {
        match value {
            Expr::Call(function, args, kwargs) => {
                let name = self.symbol(function, module, 0).unwrap_or_default();
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
                    || args.iter().any(|a| self.dynamic_global(a, module))
                    || kwargs.values().any(|a| self.dynamic_global(a, module))
            }
            Expr::Tuple(values) | Expr::Union(values) => {
                values.iter().any(|a| self.dynamic_global(a, module))
            }
            Expr::Dict(values) => values.values().any(|a| self.dynamic_global(a, module)),
            Expr::Mapping(values) => values.iter().any(|(key, value)| {
                self.dynamic_global(key, module) || self.dynamic_global(value, module)
            }),
            Expr::Subscript(_, values) => values.iter().any(|a| self.dynamic_global(a, module)),
            Expr::Other => true,
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
                let key = format!("{module}.{alias}");
                if self.aliases.contains_key(&key) || self.classes.contains_key(&key) {
                    self.unknown
                        .push(format!("{module}: imported symbol {alias} rebound"));
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
                let key = format!("{module}.{alias}");
                if self.aliases.contains_key(&key) || self.classes.contains_key(&key) {
                    self.unknown
                        .push(format!("{module}: imported symbol {alias} rebound"));
                }
                self.aliases
                    .insert(key, (Expr::Name(target.into()), String::new()));
            }
        }
    }
    fn read_class(&mut self, node: Node<'_>, file: &ModelFile, module: &str, decorated: bool) {
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
                    let meta_unsupported =
                        meta_unsupported || nested.child_by_field_name("superclasses").is_some();
                    unsupported |= meta_unsupported;
                    if meta_unsupported {
                        self.unknown.push(format!(
                            "{}:{}: dynamic nested Meta class",
                            file.path.display(),
                            nested.start_position().row + 1
                        ));
                    }
                }
            } else {
                self.unknown.push(format!(
                    "{}:{}: nested class may declare additional mappings",
                    file.path.display(),
                    nested.start_position().row + 1
                ));
            }
        }
        let bases = node
            .child_by_field_name("superclasses")
            .map_or(vec![], |n| {
                named(n).iter().map(|n| expr(*n, &file.source)).collect()
            });
        let key = format!("{module}.{}", text(name, &file.source));
        if self.aliases.remove(&key).is_some() {
            self.unknown.push(format!(
                "{}: class {key} shadows another symbol",
                file.path.display()
            ));
        }
        let class = Class {
            name: text(name, &file.source).into(),
            module: module.into(),
            orm: file.orm,
            attrs,
            meta,
            bases,
            unsupported: unsupported || decorated,
            location: format!("{}:{}", file.path.display(), node.start_position().row + 1),
        };
        if self.classes.insert(key, class).is_some() {
            self.unknown.push(format!(
                "{}: duplicate class definition",
                file.path.display()
            ));
        }
    }
    fn symbol(&self, value: &Expr, module: &str, depth: usize) -> Option<String> {
        if depth > 32 {
            return None;
        }
        let Expr::Name(name) = value else {
            return None;
        };
        let (head, suffix) = name
            .split_once('.')
            .map_or((name.as_str(), ""), |(h, s)| (h, s));
        let local = format!("{module}.{head}");
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
    }
    fn expand(&self, value: &Expr, module: &str, depth: usize) -> Expr {
        self.expand_bounded(value, module, depth, &mut 4096)
    }
    fn expand_bounded(&self, value: &Expr, module: &str, depth: usize, budget: &mut usize) -> Expr {
        if depth > 32 || *budget == 0 {
            return Expr::Limit;
        }
        *budget -= 1;
        match value {
            Expr::Name(name) => {
                if let Some((alias, origin)) = self
                    .aliases
                    .get(&format!("{module}.{name}"))
                    .or_else(|| self.aliases.get(name))
                {
                    return self.expand_bounded(alias, origin, depth + 1, budget);
                }
                Expr::Name(
                    self.symbol(value, module, 0)
                        .unwrap_or_else(|| name.clone()),
                )
            }
            Expr::Call(function, args, kwargs) => Expr::Call(
                Box::new(self.expand_bounded(function, module, depth + 1, budget)),
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1, budget))
                    .collect(),
                kwargs
                    .iter()
                    .map(|(k, v)| (k.clone(), self.expand_bounded(v, module, depth + 1, budget)))
                    .collect(),
            ),
            Expr::Dict(values) => Expr::Dict(
                values
                    .iter()
                    .map(|(k, v)| (k.clone(), self.expand_bounded(v, module, depth + 1, budget)))
                    .collect(),
            ),
            Expr::Mapping(values) => Expr::Mapping(
                values
                    .iter()
                    .map(|(key, value)| {
                        (
                            self.expand_bounded(key, module, depth + 1, budget),
                            self.expand_bounded(value, module, depth + 1, budget),
                        )
                    })
                    .collect(),
            ),
            Expr::Union(args) => Expr::Union(
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1, budget))
                    .collect(),
            ),
            Expr::Tuple(args) => Expr::Tuple(
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1, budget))
                    .collect(),
            ),
            Expr::Subscript(inner, args) => Expr::Subscript(
                Box::new(self.expand_bounded(inner, module, depth + 1, budget)),
                args.iter()
                    .map(|a| self.expand_bounded(a, module, depth + 1, budget))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }
    fn resolve_class(
        &self,
        class: &Class,
        visiting: &mut HashSet<String>,
        budget: &mut usize,
    ) -> Resolved {
        let key = format!("{}.{}", class.module, class.name);
        if *budget == 0 {
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
        *budget -= 1;
        if visiting.len() >= 128 || !visiting.insert(key.clone()) {
            return Resolved {
                model: true,
                unknown: vec![format!("{}: cyclic inheritance", class.location)],
                ..Resolved::default()
            };
        }
        let mut result = Resolved::default();
        let mut terminal_base = false;
        for base in &class.bases {
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
                let inherited = self.resolve_class(parent, visiting, budget);
                result.model |= inherited.model;
                result.table = inherited.table.or(result.table);
                result.schema = inherited.schema.or(result.schema);
                result.columns.extend(inherited.columns);
                result.unknown.extend(inherited.unknown);
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
                    let expanded = self.expand(&attr.value, &class.module, 0);
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
                "__table__" | "metadata" | "__mapper_args__" => {
                    result.identity_unknown = true;
                    result
                        .unknown
                        .push(format!("{}: unsupported mapping metadata", attr.location));
                }
                "__allow_unmapped__" | "__doc__" | "__module__" | "__qualname__" => {}
                name if name.starts_with("__") => {
                    result.identity_unknown = true;
                    result
                        .unknown
                        .push(format!("{}: unsupported mapping hook", attr.location));
                }
                _ => self.field(class, attr, &mut result),
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
            Expr::Literal => false,
            _ => true,
        }
    }
    fn field(&self, class: &Class, attr: &Attribute, result: &mut Resolved) {
        let annotation = self.expand(&attr.annotation, &class.module, 0);
        if contains_limit(&annotation) {
            result.identity_unknown = true;
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
            if attr.name == "registry" {
                let registry = self.expand(&attr.value, &class.module, 0);
                if self.dynamic_global(&registry, &class.module)
                    || !matches!(&registry, Expr::Call(function, args, kwargs) if self.symbol(function, &class.module, 0).as_deref() == Some("sqlalchemy.orm.registry") && args.is_empty() && kwargs.keys().all(|key| key == "type_annotation_map"))
                {
                    result.identity_unknown = true;
                    result.unknown.push(format!(
                        "{}: unsupported registry may alter table identity",
                        attr.location
                    ));
                }
                return;
            }
            if !matches!(
                self.expand(&attr.value, &class.module, 0),
                Expr::Call(_, _, _)
            ) {
                return;
            }
        }
        if class.orm == ModelOrm::Sqlalchemy {
            if let Expr::Subscript(wrapper, args) = &annotation {
                if self.symbol(wrapper, &class.module, 0).as_deref()
                    == Some("sqlalchemy.orm.Mapped")
                {
                    if args
                        .iter()
                        .any(|argument| self.unknown_type(argument, &class.module, false))
                    {
                        result
                            .unknown
                            .push(format!("{}: unresolved mapped type alias", attr.location));
                    }
                    for arg in args {
                        if let Expr::Subscript(inner, metadata) = arg {
                            if matches!(
                                self.symbol(inner, &class.module, 0).as_deref(),
                                Some("typing.Annotated" | "typing_extensions.Annotated")
                            ) {
                                for item in metadata.iter().skip(1) {
                                    if let Expr::Call(function, _, _) = item {
                                        if self.symbol(function, &class.module, 0).as_deref()
                                            == Some("sqlalchemy.orm.mapped_column")
                                        {
                                            let field = Attribute {
                                                name: attr.name.clone(),
                                                value: item.clone(),
                                                annotation: Expr::Other,
                                                location: attr.location.clone(),
                                            };
                                            self.field(class, &field, result);
                                            continue;
                                        }
                                    }
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
        let value = self.expand(&attr.value, &class.module, 0);
        if contains_limit(&value) {
            result.identity_unknown = true;
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
                        const FIELDS: &[&str] = &[
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
                        if FIELDS.contains(&field) {
                            column_call = Some((
                                args,
                                kwargs,
                                matches!(field, "ForeignKeyField" | "OneToOneField"),
                            ));
                        } else {
                            result.unknown.push(format!(
                                "{}: unsupported Tortoise field {field}",
                                attr.location
                            ));
                            return;
                        }
                    } else {
                        result.identity_unknown = true;
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
            let annotation = self.expand(&attr.annotation, &class.module, 0);
            if let Expr::Subscript(wrapper, arguments) = &annotation {
                let name = self.symbol(wrapper, &class.module, 0).unwrap_or_default();
                if name == "sqlalchemy.orm.Mapped" {
                    if arguments
                        .iter()
                        .any(|argument| self.unknown_type(argument, &class.module, false))
                    {
                        result
                            .unknown
                            .push(format!("{}: unresolved mapped type alias", attr.location));
                    }
                    for argument in arguments {
                        let expanded = self.expand(argument, &class.module, 0);
                        if let Expr::Subscript(inner, metadata) = expanded {
                            if matches!(
                                self.symbol(&inner, &class.module, 0).as_deref(),
                                Some("typing.Annotated" | "typing_extensions.Annotated")
                            ) {
                                for item in metadata.iter().skip(1) {
                                    if let Expr::Call(function, _, _) = item {
                                        if self.symbol(function, &class.module, 0).as_deref()
                                            == Some("sqlalchemy.orm.mapped_column")
                                        {
                                            let field = Attribute {
                                                name: attr.name.clone(),
                                                value: item.clone(),
                                                annotation: Expr::Other,
                                                location: attr.location.clone(),
                                            };
                                            self.field(class, &field, result);
                                            return;
                                        }
                                    }
                                    result.unknown.push(format!(
                                        "{}: unresolved Annotated metadata",
                                        attr.location
                                    ));
                                }
                            }
                        }
                    }
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
        if !matches!(value, Expr::String(_) | Expr::Bool(_) | Expr::Literal) {
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
            "Other = create_model()\n",
            "True and register_models()\n",
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
        for dynamic in [
            "def mapped_column(*args):\n    return renamed_column(*args)\n",
            "@register_mapping\ndef helper():\n    pass\n",
        ] {
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
