//! Shared helpers for rule unit tests.
//!
//! Every per-file rule's `#[cfg(test)] mod tests` block needed
//! the same plumbing: parse a source string with tree-sitter,
//! extract a `Migration`, build a default `Config` and
//! `RuleContext`, then invoke the rule's `check`. That helper
//! was hand-rolled in 14 rule files and would drift the moment
//! anything in the pipeline grew an argument.
//!
//! `check_rule` consolidates the per-file pipeline.
//! `check_changeset_rule` is the equivalent for `ChangesetRule`
//! (R008, R009) — it accepts the migrations and other-changed-
//! files slices directly so the per-test setup stays expressive.

use std::path::Path;

use crate::ast::Migration;
use crate::config::Config;
use crate::diagnostics::Diagnostic;

use super::{ChangesetRule, Rule, RuleContext};

/// Run a per-file rule against an in-memory migration source.
/// Uses a default `Config` and `RuleContext { path: "test.py" }`.
pub(crate) fn check_rule<R: Rule>(rule: &R, source: &str) -> Vec<Diagnostic> {
    let migration = Migration::from_source(Path::new("test.py"), source)
        .expect("test source should parse and extract");
    let config = Config::default();
    let ctx = RuleContext {
        config: &config,
        path: Path::new("test.py"),
    };
    rule.check(&migration, &ctx)
}

/// Run a changeset rule against pre-built migration slices and
/// other-changed-files. The caller owns Migration and Path
/// allocation; this helper just packages the boilerplate
/// RuleContext at the path-less "." root.
pub(crate) fn check_changeset_rule<R: ChangesetRule>(
    rule: &R,
    migrations: &[&Migration],
    other_files: &[&Path],
    config: &Config,
) -> Vec<Diagnostic> {
    let ctx = RuleContext {
        config,
        path: Path::new("."),
    };
    rule.check(migrations, other_files, &ctx)
}
