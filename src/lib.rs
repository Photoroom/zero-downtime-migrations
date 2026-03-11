//! Zero-Downtime Migrations (zdm) - A PostgreSQL migration safety linter for Django
//!
//! This library provides static analysis of Django migration files to catch unsafe
//! patterns that cause table locks, outages, and data loss on large PostgreSQL databases.

pub mod ast;
pub mod config;
pub mod diagnostics;
pub mod discovery;
pub mod error;
pub mod git;
pub mod parser;
pub mod rules;

pub use error::{Error, Result};
