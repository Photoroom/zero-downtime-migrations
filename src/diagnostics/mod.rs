//! Diagnostic types for reporting lint violations.
//!
//! This module provides the types used to represent lint findings,
//! including source spans, severity levels, and formatting.

mod diagnostic;
mod severity;
mod span;

pub use diagnostic::{Diagnostic, Diagnostics, Fix};
pub use severity::Severity;
pub use span::Span;
