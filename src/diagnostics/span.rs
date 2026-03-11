//! Source spans for locating diagnostics in files.

use std::ops::Range;

/// A span representing a range in source code.
///
/// Spans are used to highlight the exact location of an issue
/// in the migration file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// Byte offset of the start of the span (0-indexed).
    pub start: usize,
    /// Byte offset of the end of the span (exclusive).
    pub end: usize,
    /// 1-indexed line number where the span starts.
    pub start_line: usize,
    /// 0-indexed column number where the span starts.
    pub start_column: usize,
    /// 1-indexed line number where the span ends.
    pub end_line: usize,
    /// 0-indexed column number where the span ends.
    pub end_column: usize,
}

impl Span {
    /// Create a new span from byte offsets and line/column information.
    pub fn new(
        start: usize,
        end: usize,
        start_line: usize,
        start_column: usize,
        end_line: usize,
        end_column: usize,
    ) -> Self {
        Self {
            start,
            end,
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    /// Create a span from a tree-sitter node.
    pub fn from_node(node: &tree_sitter::Node) -> Self {
        let start_pos = node.start_position();
        let end_pos = node.end_position();

        Self {
            start: node.start_byte(),
            end: node.end_byte(),
            start_line: start_pos.row + 1, // tree-sitter uses 0-indexed rows
            start_column: start_pos.column,
            end_line: end_pos.row + 1,
            end_column: end_pos.column,
        }
    }

    /// Returns the byte range of this span.
    pub fn byte_range(&self) -> Range<usize> {
        self.start..self.end
    }

    /// Returns true if this span is on a single line.
    pub fn is_single_line(&self) -> bool {
        self.start_line == self.end_line
    }

    /// Returns the length in bytes.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Returns true if the span is empty.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl Default for Span {
    fn default() -> Self {
        Self {
            start: 0,
            end: 0,
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 0,
        }
    }
}
