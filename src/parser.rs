//! Python parser for Django migration files using tree-sitter.
//!
//! This module provides low-level parsing of Python migration files,
//! extracting the raw AST nodes. The `ast` module then converts these
//! into typed Rust structures.

use std::path::Path;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node, Parser, Tree};

use crate::error::{Error, Result};

/// Maximum file size for migration files (10 MB).
/// This bounds memory use and parse time when processing untrusted input.
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Returns `Err(Error::FileTooLarge)` if the given byte count exceeds
/// `MAX_FILE_SIZE`. Callers that already hold a source string should pass
/// `source.len() as u64`; callers reading from disk can stat the file first.
pub fn check_size(path: &Path, size: u64) -> Result<()> {
    if size > MAX_FILE_SIZE {
        return Err(Error::file_too_large(path, size, MAX_FILE_SIZE));
    }
    Ok(())
}

/// Global Python language instance.
static PYTHON_LANGUAGE: Lazy<Language> = Lazy::new(|| tree_sitter_python::LANGUAGE.into());

/// A parsed Python migration file.
#[derive(Debug)]
pub struct ParsedMigration {
    /// The source code.
    pub source: String,
    /// The tree-sitter parse tree.
    tree: Tree,
}

impl ParsedMigration {
    /// Parse a migration file from source code.
    pub fn parse(source: impl Into<String>) -> Result<Self> {
        let source = source.into();
        let mut parser = Parser::new();
        parser
            .set_language(&PYTHON_LANGUAGE)
            .expect("Failed to set Python language");

        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| Error::parse_error("<source>", "tree-sitter failed to parse"))?;

        Ok(Self { source, tree })
    }

    /// Parse a migration file from a path.
    pub fn parse_file(path: &Path) -> Result<Self> {
        // Cap on-disk size before reading to bound memory.
        let metadata = std::fs::metadata(path).map_err(|e| Error::file_read(path, e))?;
        check_size(path, metadata.len())?;

        let source = std::fs::read_to_string(path).map_err(|e| Error::file_read(path, e))?;

        let mut parser = Parser::new();
        parser
            .set_language(&PYTHON_LANGUAGE)
            .expect("Failed to set Python language");

        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| Error::parse_error(path, "tree-sitter failed to parse"))?;

        // Check for parse errors
        if tree.root_node().has_error() {
            // Find the first error node to report location
            if let Some(error_node) = find_error_node(tree.root_node()) {
                return Err(Error::parse_error_with_location(
                    path,
                    error_node.start_position().row + 1,
                    error_node.start_position().column,
                ));
            }
        }

        Ok(Self { source, tree })
    }

    /// Get the root node of the parse tree.
    pub fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// Get the source code as bytes.
    pub fn source_bytes(&self) -> &[u8] {
        self.source.as_bytes()
    }

    /// Check if the parse tree has any errors.
    pub fn has_errors(&self) -> bool {
        self.tree.root_node().has_error()
    }

    /// Find the Migration class node, if present.
    pub fn find_migration_class(&self) -> Option<Node<'_>> {
        let root = self.root_node();
        let source = self.source_bytes();

        for child in root.children(&mut root.walk()) {
            if child.kind() == "class_definition" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    if name_node.utf8_text(source).ok() == Some("Migration") {
                        return Some(child);
                    }
                }
            }
        }
        None
    }

    /// Find the operations list node, if present.
    pub fn find_operations_list(&self) -> Option<Node<'_>> {
        let class_node = self.find_migration_class()?;
        let source = self.source_bytes();

        let body = class_node.child_by_field_name("body")?;

        for child in body.children(&mut body.walk()) {
            if child.kind() == "expression_statement" {
                if let Some(assignment) = child.child(0) {
                    if assignment.kind() == "assignment" {
                        if let Some(left) = assignment.child_by_field_name("left") {
                            if left.utf8_text(source).ok() == Some("operations") {
                                return assignment.child_by_field_name("right");
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Check if the migration has `atomic = False` as a class-body
    /// assignment. Only matches a real assignment whose LHS is the bare
    /// identifier `atomic` and whose RHS is the Python `False` literal
    /// (including the parenthesized form `(False)`). Comments mentioning
    /// "False", strings containing those words, `atomic = True`,
    /// `not_atomic = False`, and `atomic = some_func(False)` do not match.
    pub fn is_non_atomic(&self) -> bool {
        let Some(class_node) = self.find_migration_class() else {
            return false;
        };
        let source = self.source_bytes();

        let Some(body) = class_node.child_by_field_name("body") else {
            return false;
        };

        for child in body.children(&mut body.walk()) {
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
            if left.utf8_text(source).ok() != Some("atomic") {
                continue;
            }
            let Some(right) = assignment.child_by_field_name("right") else {
                continue;
            };
            if is_false_literal(right) {
                return true;
            }
        }
        false
    }

    /// Collect every comment node in the parse tree, returning `(line,
    /// text)` pairs. Lines are 1-indexed. The comment text retains its
    /// leading `#`. Used by the inline-ignore (`# zdm: ignore RXXX`)
    /// machinery so rules can be suppressed at specific source lines.
    pub fn all_comments(&self) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        self.collect_comments(self.root_node(), &mut out);
        out
    }

    fn collect_comments(&self, node: Node<'_>, out: &mut Vec<(usize, String)>) {
        if node.kind() == "comment" {
            let line = node.start_position().row + 1;
            let text = node
                .utf8_text(self.source_bytes())
                .unwrap_or("")
                .to_string();
            out.push((line, text));
            return;
        }
        for child in node.children(&mut node.walk()) {
            self.collect_comments(child, out);
        }
    }

    /// Get all import statements in the file.
    pub fn get_imports(&self) -> Vec<Node<'_>> {
        let root = self.root_node();
        let mut imports = Vec::new();

        for child in root.children(&mut root.walk()) {
            if child.kind() == "import_statement" || child.kind() == "import_from_statement" {
                imports.push(child);
            }
        }

        imports
    }

    /// Get the text of a node.
    pub fn node_text(&self, node: Node<'_>) -> &str {
        node.utf8_text(self.source_bytes()).unwrap_or("")
    }
}

/// Whether a node is the Python `False` literal, possibly wrapped in any
/// number of parentheses. tree-sitter-python represents `False` with the
/// `"false"` node kind and `(expr)` as `parenthesized_expression`.
fn is_false_literal(node: Node<'_>) -> bool {
    let mut current = node;
    loop {
        match current.kind() {
            "false" => return true,
            "parenthesized_expression" => match current.named_child(0) {
                Some(inner) => current = inner,
                None => return false,
            },
            _ => return false,
        }
    }
}

/// Find the first error node in the tree.
fn find_error_node(node: Node<'_>) -> Option<Node<'_>> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }

    for child in node.children(&mut node.walk()) {
        if let Some(error) = find_error_node(child) {
            return Some(error);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_MIGRATION: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        migrations.AddIndex(
            model_name='order',
            index=models.Index(fields=['created_at'], name='order_created_idx'),
        ),
    ]
"#;

    const NON_ATOMIC_MIGRATION: &str = r#"
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):

    atomic = False

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['created_at'], name='order_idx'),
        ),
    ]
"#;

    const INVALID_PYTHON: &str = r#"
from django.db import migrations

class Migration(migrations.Migration)  # Missing colon
    operations = []
"#;

    #[test]
    fn test_parse_simple_migration() {
        let parsed = ParsedMigration::parse(SIMPLE_MIGRATION).unwrap();
        assert!(!parsed.has_errors());
    }

    #[test]
    fn test_find_migration_class() {
        let parsed = ParsedMigration::parse(SIMPLE_MIGRATION).unwrap();
        let class_node = parsed.find_migration_class();
        assert!(class_node.is_some());
    }

    #[test]
    fn test_find_operations_list() {
        let parsed = ParsedMigration::parse(SIMPLE_MIGRATION).unwrap();
        let ops_node = parsed.find_operations_list();
        assert!(ops_node.is_some());
        assert_eq!(ops_node.unwrap().kind(), "list");
    }

    #[test]
    fn test_is_non_atomic() {
        let atomic = ParsedMigration::parse(SIMPLE_MIGRATION).unwrap();
        assert!(!atomic.is_non_atomic());

        let non_atomic = ParsedMigration::parse(NON_ATOMIC_MIGRATION).unwrap();
        assert!(non_atomic.is_non_atomic());
    }

    #[test]
    fn test_is_non_atomic_accepts_parenthesized_false() {
        let source = r#"
class Migration:
    atomic = (False)
    operations = []
"#;
        let parsed = ParsedMigration::parse(source).unwrap();
        assert!(parsed.is_non_atomic());

        let nested = r#"
class Migration:
    atomic = ((False))
    operations = []
"#;
        let parsed = ParsedMigration::parse(nested).unwrap();
        assert!(parsed.is_non_atomic());
    }

    #[test]
    fn test_is_non_atomic_rejects_substring_lookalikes() {
        // None of these should be detected as non-atomic, even though the old
        // substring scan ("text contains 'atomic' and 'False'") would have
        // matched several of them.
        let cases: &[(&str, &str)] = &[
            (
                "atomic = True",
                r#"
class Migration:
    atomic = True
    operations = []
"#,
            ),
            (
                "not_atomic = False",
                r#"
class Migration:
    not_atomic = False
    operations = []
"#,
            ),
            (
                "atomic = some_func(False)",
                r#"
class Migration:
    atomic = some_func(False)
    operations = []
"#,
            ),
            (
                "atomic appears only in a string",
                r#"
class Migration:
    description = "atomic was set to False elsewhere"
    operations = []
"#,
            ),
            (
                "atomic appears only in a comment",
                r#"
class Migration:
    # atomic = False would disable transactions
    operations = []
"#,
            ),
        ];
        for (label, source) in cases {
            let parsed = ParsedMigration::parse(*source).unwrap();
            assert!(
                !parsed.is_non_atomic(),
                "case `{label}` should not be flagged as non-atomic"
            );
        }
    }

    #[test]
    fn test_get_imports() {
        let parsed = ParsedMigration::parse(NON_ATOMIC_MIGRATION).unwrap();
        let imports = parsed.get_imports();
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn test_parse_error_detection() {
        let parsed = ParsedMigration::parse(INVALID_PYTHON).unwrap();
        assert!(parsed.has_errors());
    }

    #[test]
    fn test_node_text() {
        let parsed = ParsedMigration::parse(SIMPLE_MIGRATION).unwrap();
        let class_node = parsed.find_migration_class().unwrap();

        // The class name should be extractable
        let name_node = class_node.child_by_field_name("name").unwrap();
        assert_eq!(parsed.node_text(name_node), "Migration");
    }

    #[test]
    fn test_operations_children() {
        let parsed = ParsedMigration::parse(SIMPLE_MIGRATION).unwrap();
        let ops = parsed.find_operations_list().unwrap();

        // Count actual operation calls (not brackets/commas)
        let mut call_count = 0;
        for child in ops.children(&mut ops.walk()) {
            if child.kind() == "call" {
                call_count += 1;
            }
        }
        assert_eq!(call_count, 1);
    }

    #[test]
    fn test_check_size_accepts_under_limit() {
        assert!(check_size(Path::new("test.py"), 0).is_ok());
        assert!(check_size(Path::new("test.py"), MAX_FILE_SIZE).is_ok());
    }

    #[test]
    fn test_check_size_rejects_over_limit() {
        let err = check_size(Path::new("huge.py"), MAX_FILE_SIZE + 1).unwrap_err();
        match err {
            Error::FileTooLarge {
                path,
                size,
                max_size,
            } => {
                assert_eq!(path, Path::new("huge.py"));
                assert_eq!(size, MAX_FILE_SIZE + 1);
                assert_eq!(max_size, MAX_FILE_SIZE);
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }
}
