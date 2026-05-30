//! Python parser for Django migration files using tree-sitter.
//!
//! This module provides low-level parsing of Python migration files,
//! extracting the raw AST nodes. The `ast` module then converts these
//! into typed Rust structures.

use std::path::Path;
use std::sync::LazyLock;

use tree_sitter::{Language, Node, Parser, Tree};

use crate::error::{Error, Result};
use crate::file_io::{read_bounded_regular_file, ReadFileError};

/// Maximum size (in bytes) of a single migration file the
/// parser will accept. Bounds memory use and parse time when
/// processing untrusted input. Currently a hard-coded 10 MiB.
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Returns `Err(Error::FileTooLarge)` if the given byte count exceeds
/// `MAX_FILE_SIZE`. Callers that already hold a source string should pass
/// `source.len() as u64`; callers reading from disk can stat the file first.
pub(crate) fn check_size(path: &Path, size: u64) -> Result<()> {
    if size > MAX_FILE_SIZE {
        return Err(Error::file_too_large(path, size, MAX_FILE_SIZE));
    }
    Ok(())
}

/// Global Python language instance.
static PYTHON_LANGUAGE: LazyLock<Language> = LazyLock::new(|| tree_sitter_python::LANGUAGE.into());

/// A parsed Python migration file.
#[derive(Debug)]
pub struct ParsedMigration {
    /// The source code.
    pub(crate) source: String,
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
            .ok_or_else(|| Error::parse("<source>", "tree-sitter failed to parse"))?;

        Ok(Self { source, tree })
    }

    /// Parse a migration file from a path.
    pub fn parse_file(path: &Path) -> Result<Self> {
        let source = match read_bounded_regular_file(path, MAX_FILE_SIZE) {
            Ok(source) => source,
            Err(ReadFileError::Io(error)) => return Err(Error::file_read(path, error)),
            Err(ReadFileError::TooLarge { size, max }) => {
                return Err(Error::file_too_large(path, size, max));
            }
        };

        let mut parser = Parser::new();
        parser
            .set_language(&PYTHON_LANGUAGE)
            .expect("Failed to set Python language");

        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| Error::parse(path, "tree-sitter failed to parse"))?;

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
    pub(crate) fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// Get the source code as bytes.
    pub(crate) fn source_bytes(&self) -> &[u8] {
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
    pub(crate) fn find_operations_list(&self) -> Option<Node<'_>> {
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
    pub(crate) fn get_imports(&self) -> Vec<Node<'_>> {
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
    pub(crate) fn node_text(&self, node: Node<'_>) -> &str {
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
    fn test_parse_file_surfaces_parse_error_with_location() {
        // `parse()` only flags errors via `has_errors()`; `parse_file`
        // additionally raises them as `Error::ParseErrorWithLocation`
        // so the CLI can render an actionable diagnostic. This path
        // was previously untested — a refactor that silently swallowed
        // the error would have shipped.
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(INVALID_PYTHON.as_bytes()).unwrap();
        let path = file.path().to_path_buf();

        let err = ParsedMigration::parse_file(&path).unwrap_err();
        let (err_path, line) = match err {
            Error::ParseErrorWithLocation { path, line, .. } => (path, line),
            other => panic!("expected ParseErrorWithLocation, got: {other:?}"),
        };
        assert_eq!(err_path, path, "error should reference the file we parsed");
        // INVALID_PYTHON starts with a leading newline, then 3 lines
        // of valid code, then the `class Migration(... )` line that
        // is missing its colon (line 4 of the literal string, which
        // means the error node starts somewhere on or after line 4).
        // The broken class header is line 4 of INVALID_PYTHON
        // (1: empty leading newline, 2: import, 3: blank, 4: class
        // header missing its colon). Tree-sitter's recovery anchors
        // at the error node, which can't predate the broken header
        // because lines 1-3 parse cleanly. Accepting line 3 (blank
        // line, valid) or earlier would mean the location wasn't
        // extracted from the error node at all.
        assert!(
            (4..=INVALID_PYTHON.lines().count() + 1).contains(&line),
            "error line {line} should fall on or after the broken class header (line 4)",
        );
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

    // Module-boundary edge cases for `find_migration_class`. Each test
    // pins the chosen behaviour so a refactor of the AST walk can't
    // silently change which class (if any) the rule engine ends up
    // analysing. Without these pins, a "look at the second class" or
    // "recurse into nested classes" change would slip through every
    // existing rule test (which assumes a well-formed single-Migration
    // file).

    #[test]
    fn test_find_migration_class_returns_none_for_empty_source() {
        let parsed = ParsedMigration::parse("").unwrap();
        assert!(parsed.find_migration_class().is_none());
    }

    #[test]
    fn test_find_migration_class_returns_none_for_comments_only() {
        let parsed = ParsedMigration::parse("# just a comment\n# and another\n").unwrap();
        assert!(parsed.find_migration_class().is_none());
    }

    #[test]
    fn test_find_migration_class_returns_none_when_no_migration_class() {
        // Other top-level classes exist but none named `Migration`.
        let source = r#"
from django.db import migrations


class NotAMigration:
    operations = []


class HelperClass:
    pass
"#;
        let parsed = ParsedMigration::parse(source).unwrap();
        assert!(parsed.find_migration_class().is_none());
    }

    #[test]
    fn test_find_migration_class_returns_first_when_multiple() {
        // Pathological but legal Python: two top-level `class Migration`
        // definitions. The second shadows the first at runtime, but
        // Django itself doesn't allow this — `makemigrations` writes
        // exactly one. Pin that the parser returns the FIRST so a
        // refactor that switches to "last" surfaces the change.
        let source = r#"
from django.db import migrations


class Migration(migrations.Migration):
    operations = ['FIRST']


class Migration(migrations.Migration):  # noqa: F811
    operations = ['SECOND']
"#;
        let parsed = ParsedMigration::parse(source).unwrap();
        let class_node = parsed.find_migration_class().expect("class found");
        let class_text = &source[class_node.byte_range()];
        assert!(
            class_text.contains("'FIRST'"),
            "find_migration_class should return the first definition, got:\n{class_text}",
        );
    }

    #[test]
    fn test_find_migration_class_ignores_nested_migration_class() {
        // A `class Migration:` nested inside another class is not the
        // Django Migration — `find_migration_class` only walks
        // top-level children of the module root.
        let source = r#"
class Container:
    class Migration:
        operations = []
"#;
        let parsed = ParsedMigration::parse(source).unwrap();
        assert!(parsed.find_migration_class().is_none());
    }

    #[test]
    fn test_find_migration_class_skips_shebang() {
        // A leading shebang line (rare but legal in Python source)
        // should not prevent the parser from finding the class.
        let source = "#!/usr/bin/env python\n\nclass Migration:\n    operations = []\n";
        let parsed = ParsedMigration::parse(source).unwrap();
        assert!(parsed.find_migration_class().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_file_rejects_symlink_target() {
        // `std::fs::metadata` follows symlinks, so a symlink to
        // `/dev/zero` would report length 0 and skip the size cap,
        // then OOM at `read_to_string`. The fix uses
        // `symlink_metadata` and rejects any non-regular file
        // outright. Pin both: the rejection fires, AND the error
        // is the InvalidInput we constructed (so a future refactor
        // that swaps the rejection out for a silent file-read
        // surfaces here).
        use std::os::unix::fs::symlink;
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("real.py");
        std::fs::write(&target, "class Migration:\n    operations = []\n").unwrap();
        let link = temp.path().join("link.py");
        symlink(&target, &link).unwrap();

        let result = ParsedMigration::parse_file(&link);
        match result {
            Err(Error::FileRead { source, .. }) => {
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
                assert!(
                    source.to_string().contains("non-regular file"),
                    "expected non-regular-file rejection, got: {source}",
                );
            }
            other => panic!("expected FileRead InvalidInput, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_file_accepts_real_file_next_to_symlink() {
        // Symmetry pin: the target of the symlink is a perfectly
        // valid regular file and must parse if accessed directly,
        // so the rejection is specifically about the symlink
        // itself — not about any path in the same dir.
        use std::os::unix::fs::symlink;
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("real.py");
        std::fs::write(&target, "class Migration:\n    operations = []\n").unwrap();
        let link = temp.path().join("link.py");
        symlink(&target, &link).unwrap();

        // The link is rejected.
        assert!(ParsedMigration::parse_file(&link).is_err());
        // The target is accepted.
        assert!(ParsedMigration::parse_file(&target).is_ok());
    }
}
