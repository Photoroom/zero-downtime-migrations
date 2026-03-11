//! Common test utilities for zero-downtime-migrations.
//!
//! This module provides:
//! - Fixture loading utilities
//! - Test harness for running rules against fixtures
//! - Snapshot testing helpers

use std::path::{Path, PathBuf};
use std::fs;

/// Get the path to the test fixtures directory.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Get the path to a specific rule's fixtures directory.
pub fn rule_fixtures_dir(rule_id: &str) -> PathBuf {
    fixtures_dir().join("rules").join(rule_id)
}

/// Load a fixture file's contents.
pub fn load_fixture(rule_id: &str, filename: &str) -> String {
    let path = rule_fixtures_dir(rule_id).join(filename);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to load fixture {}: {}", path.display(), e))
}

/// List all fixture files for a rule.
pub fn list_fixtures(rule_id: &str) -> Vec<PathBuf> {
    let dir = rule_fixtures_dir(rule_id);
    if !dir.exists() {
        return vec![];
    }

    fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("Failed to read fixtures dir {}: {}", dir.display(), e))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map_or(false, |ext| ext == "py"))
        .collect()
}

/// A test case for a rule.
#[derive(Debug)]
pub struct RuleTestCase {
    /// The rule ID being tested.
    pub rule_id: String,
    /// The fixture filename.
    pub filename: String,
    /// The fixture file path.
    pub path: PathBuf,
    /// The fixture source code.
    pub source: String,
    /// Whether this test case should pass (no violations).
    pub should_pass: bool,
}

impl RuleTestCase {
    /// Load a test case from a fixture file.
    ///
    /// Naming convention:
    /// - `pass_*.py` - should produce no violations
    /// - `fail_*.py` - should produce violations
    pub fn load(rule_id: &str, filename: &str) -> Self {
        let path = rule_fixtures_dir(rule_id).join(filename);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to load fixture {}: {}", path.display(), e));

        let should_pass = filename.starts_with("pass_");

        Self {
            rule_id: rule_id.to_string(),
            filename: filename.to_string(),
            path,
            source,
            should_pass,
        }
    }

    /// Load all test cases for a rule.
    pub fn load_all(rule_id: &str) -> Vec<Self> {
        list_fixtures(rule_id)
            .into_iter()
            .filter_map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| Self::load(rule_id, name))
            })
            .collect()
    }
}

/// Macro for generating snapshot tests for a rule.
///
/// Usage:
/// ```ignore
/// snapshot_test_rule!(R001, "fail_non_concurrent_add_index.py");
/// ```
#[macro_export]
macro_rules! snapshot_test_rule {
    ($rule_id:ident, $fixture:expr) => {
        paste::paste! {
            #[test]
            fn [<test_ $rule_id:lower _ $fixture:snake>]() {
                let test_case = $crate::common::RuleTestCase::load(
                    stringify!($rule_id),
                    $fixture,
                );

                // TODO: Run the rule and snapshot the diagnostics
                insta::assert_yaml_snapshot!(test_case.filename, {
                    ".path" => "[path]",
                });
            }
        }
    };
}
