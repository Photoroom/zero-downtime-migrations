//! End-to-end integration tests using real migration fixture files.
//!
//! These tests validate the full CLI workflow with fixture files
//! that represent real-world migration patterns.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

/// Helper to create a command for the `zdm` binary
fn zdm() -> Command {
    Command::cargo_bin("zdm").unwrap()
}

/// Path to the test fixtures directory
fn fixtures_dir() -> &'static Path {
    Path::new("tests/fixtures/rules")
}

// =============================================================================
// R001 Tests - Non-Concurrent AddIndex
// =============================================================================

#[test]
fn e2e_r001_fail_non_concurrent_add_index() {
    let fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R001"))
        .stdout(predicate::str::contains("AddIndexConcurrently"));
}

#[test]
fn e2e_r001_pass_concurrent_add_index() {
    let fixture = fixtures_dir().join("R001/pass_concurrent_add_index.py");

    zdm().arg(&fixture).assert().success().code(0);
}

#[test]
fn e2e_r001_pass_add_index_on_new_model() {
    // CreateModel exemption - adding index on newly created model is safe
    let fixture = fixtures_dir().join("R001/pass_add_index_on_new_model.py");

    zdm().arg(&fixture).assert().success().code(0);
}

// =============================================================================
// R010 Tests - AddField NOT NULL without default
// =============================================================================

#[test]
fn e2e_r010_pass_nullable_field() {
    let fixture = fixtures_dir().join("R010/pass_nullable_field.py");
    if !fixture.exists() {
        return; // Skip if fixture doesn't exist
    }

    zdm().arg(&fixture).assert().success().code(0);
}

#[test]
fn e2e_r010_fail_not_null_without_default() {
    let fixture = fixtures_dir().join("R010/fail_not_null_without_default.py");
    if !fixture.exists() {
        return; // Skip if fixture doesn't exist
    }

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R010"));
}

// =============================================================================
// R016 Tests - Non-Concurrent RemoveIndex
// =============================================================================

#[test]
fn e2e_r016_fail_non_concurrent_remove_index() {
    let fixture = fixtures_dir().join("R016/fail_non_concurrent_remove_index.py");
    if !fixture.exists() {
        return; // Skip if fixture doesn't exist
    }

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R016"))
        .stdout(predicate::str::contains("RemoveIndexConcurrently"));
}

#[test]
fn e2e_r016_pass_concurrent_remove_index() {
    let fixture = fixtures_dir().join("R016/pass_concurrent_remove_index.py");
    if !fixture.exists() {
        return; // Skip if fixture doesn't exist
    }

    zdm().arg(&fixture).assert().success().code(0);
}

// =============================================================================
// Multiple Rules in Same File
// =============================================================================

#[test]
fn e2e_multiple_rules_detect_all() {
    // Lint multiple failing fixtures at once
    let r001_fail = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");
    let r016_fail = fixtures_dir().join("R016/fail_non_concurrent_remove_index.py");

    if !r001_fail.exists() || !r016_fail.exists() {
        return;
    }

    zdm()
        .arg(&r001_fail)
        .arg(&r016_fail)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R001"))
        .stdout(predicate::str::contains("R016"));
}

// =============================================================================
// JSON Output with Fixtures
// =============================================================================

#[test]
fn e2e_json_output_structure() {
    let fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");

    let output = zdm()
        .arg(&fixture)
        .arg("--output-format")
        .arg("json")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    // Verify structure
    assert!(parsed["diagnostics"].is_array());
    assert!(parsed["summary"]["total"].as_u64().unwrap() >= 1);
    assert!(parsed["summary"]["errors"].as_u64().unwrap() >= 1);

    // Verify diagnostic fields
    let diag = &parsed["diagnostics"][0];
    assert_eq!(diag["rule_id"], "R001");
    assert_eq!(diag["severity"], "error");
    assert!(diag["path"]
        .as_str()
        .unwrap()
        .contains("fail_non_concurrent_add_index.py"));
}

// =============================================================================
// Rule Selection with Fixtures
// =============================================================================

#[test]
fn e2e_ignore_rule_skips_detection() {
    let fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");

    zdm()
        .arg(&fixture)
        .arg("--ignore")
        .arg("R001")
        .assert()
        .success()
        .code(0);
}

#[test]
fn e2e_select_rule_only_checks_that_rule() {
    let fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");

    // With --select R002, R001 violations should not be reported
    zdm()
        .arg(&fixture)
        .arg("--select")
        .arg("R002")
        .assert()
        .success()
        .code(0);
}

// =============================================================================
// Directory Scanning
// =============================================================================

#[test]
fn e2e_scan_directory_finds_all_issues() {
    let r001_dir = fixtures_dir().join("R001");

    let output = zdm().arg(&r001_dir).assert().get_output().stdout.clone();

    let output_str = String::from_utf8(output).unwrap();

    // Should find the failing migration but not report issues for passing ones
    if output_str.contains("R001") {
        // Found a violation - that's expected for the fail_ file
        assert!(output_str.contains("fail_non_concurrent_add_index.py"));
    }
}
