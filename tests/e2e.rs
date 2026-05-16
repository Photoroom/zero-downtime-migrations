//! End-to-end integration tests using real migration fixture files.
//!
//! These tests validate the full CLI workflow with fixture files
//! that represent real-world migration patterns.

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

/// Helper to create a command for the `zdm` binary
fn zdm() -> Command {
    cargo_bin_cmd!("zdm")
}

/// Path to the per-rule test fixtures directory.
fn fixtures_dir() -> &'static Path {
    Path::new("tests/fixtures/rules")
}

/// Path to the test fixtures root (parent of `rules/` and other fixture trees).
fn fixtures_root() -> &'static Path {
    Path::new("tests/fixtures")
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
    // Pass-only assertions like `.success().code(0)` are
    // self-confirming: if the entire rule registry silently
    // stopped emitting diagnostics on this fixture (or any
    // fixture), the test would still pass. Pair every pass
    // fixture with a sibling lint of the matching fail fixture
    // to prove the rule is alive on this branch.
    let pass_fixture = fixtures_dir().join("R001/pass_concurrent_add_index.py");
    zdm().arg(&pass_fixture).assert().success().code(0);

    let fail_fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");
    zdm()
        .arg(&fail_fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R001"));
}

#[test]
fn e2e_r001_pass_add_index_on_new_model() {
    // CreateModel exemption — adding index on newly created
    // model is safe. Paired with the same fail-fixture sibling
    // as above to defend against silent registry regressions.
    let pass_fixture = fixtures_dir().join("R001/pass_add_index_on_new_model.py");
    zdm().arg(&pass_fixture).assert().success().code(0);

    let fail_fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");
    zdm()
        .arg(&fail_fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R001"));
}

// =============================================================================
// R010 Tests - AddField NOT NULL without default
// =============================================================================

#[test]
fn e2e_r010_pass_nullable_field() {
    // Paired with the fail fixture (see e2e_r001 pass tests for
    // rationale) so a silent registry regression on R010 can't
    // pass this test by emitting nothing.
    let pass_fixture = fixtures_dir().join("R010/pass_nullable_field.py");
    zdm().arg(&pass_fixture).assert().success().code(0);

    let fail_fixture = fixtures_dir().join("R010/fail_add_field_not_null.py");
    zdm()
        .arg(&fail_fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R010"));
}

#[test]
fn e2e_r010_fail_not_null_without_default() {
    let fixture = fixtures_dir().join("R010/fail_add_field_not_null.py");

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
    let fixture = fixtures_dir().join("R016/fail_remove_index_non_concurrent.py");

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
    // Paired with the fail fixture so a silent registry
    // regression on R016 can't pass this test by emitting
    // nothing.
    let pass_fixture = fixtures_dir().join("R016/pass_concurrent_remove_index.py");
    zdm().arg(&pass_fixture).assert().success().code(0);

    let fail_fixture = fixtures_dir().join("R016/fail_remove_index_non_concurrent.py");
    zdm()
        .arg(&fail_fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R016"));
}

// =============================================================================
// Coverage for previously-orphan fixtures.
//
// These fixture files lived on disk but had no e2e test referencing them
// before this PR, so they masqueraded as coverage. The tests below pin
// each one to a concrete expected outcome so a regression in the rule
// will surface here. R012 is the only Warning in the set; everything
// else is Error (exit 1).
// =============================================================================

#[test]
fn e2e_r002_fail_unique_constraint() {
    let fixture = fixtures_dir().join("R002/fail_unique_constraint.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R002"));
}

#[test]
fn e2e_r003_fail_run_sql_create_index() {
    let fixture = fixtures_dir().join("R003/fail_run_sql_create_index.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R003"))
        .stdout(predicate::str::contains("CONCURRENTLY"));
}

#[test]
fn e2e_r006_fail_add_field_fk() {
    let fixture = fixtures_dir().join("R006/fail_add_field_fk.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R006"));
}

#[test]
fn e2e_r011_fail_rename_field() {
    let fixture = fixtures_dir().join("R011/fail_rename_field.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R011"));
}

#[test]
fn e2e_r012_fail_run_python_irreversible() {
    // R012 is a Warning, so the run still exits 0 unless the user opts
    // into --warnings-as-errors; assert both the warning surfaces and
    // the exit code reflects the intended severity policy.
    let fixture = fixtures_dir().join("R012/fail_run_python_irreversible.py");

    zdm()
        .arg(&fixture)
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("R012"));
}

#[test]
fn e2e_r014_fail_model_import() {
    // The fixture lacks a reverse_code on its RunPython, so R012
    // (Warning) fires alongside R014 (Error). Pin BOTH rule IDs
    // so the test catches:
    //   - R014 silently stops firing (was pinned before)
    //   - R012 silently stops firing on this fixture (would have
    //     passed the old `contains("R014")` check while masking
    //     a real regression in R012's RunPython detection)
    // Also use `--select R014,R012` to scope the run so a future
    // false-positive in some other rule (R010 etc.) on this same
    // fixture doesn't blur the assertion.
    let fixture = fixtures_dir().join("R014/fail_model_import.py");

    zdm()
        .arg(&fixture)
        .arg("--select")
        .arg("R014,R012")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R014"))
        .stdout(predicate::str::contains("R012"));
}

#[test]
fn e2e_oversized_file_is_rejected_with_parse_error_exit_code() {
    // The parser caps inputs at `MAX_FILE_SIZE` to bound memory. A
    // file just over the cap should be rejected before tree-sitter
    // sees it; the CLI surfaces the rejection as a parse error and
    // exits 2. Derive the fixture size from the constant so a
    // future cap change doesn't silently desync the test (we'd
    // ship a fixture below the new cap and stop exercising the
    // rejection path).
    use std::fs::File;
    use std::io::Write;
    use zero_downtime_migrations::parser::MAX_FILE_SIZE;

    let temp = tempfile::TempDir::new().unwrap();
    // Plant `.git` so the config walk-up stays scoped to the temp
    // dir. The current test only exercises parser-level rejection,
    // but the boundary pin keeps it robust to future code paths
    // that load config during single-file invocations.
    std::fs::create_dir_all(temp.path().join(".git")).unwrap();
    let migrations_dir = temp.path().join("app").join("migrations");
    std::fs::create_dir_all(&migrations_dir).unwrap();
    std::fs::write(migrations_dir.join("__init__.py"), "").unwrap();

    let huge_path = migrations_dir.join("0001_huge.py");
    let mut file = File::create(&huge_path).unwrap();
    // Exactly `MAX_FILE_SIZE + 1` bytes — the smallest input that
    // trips the cap.
    let chunk = vec![b'#'; 4096];
    let target = MAX_FILE_SIZE + 1;
    let mut written = 0u64;
    while written + chunk.len() as u64 <= target {
        file.write_all(&chunk).unwrap();
        written += chunk.len() as u64;
    }
    let remainder = (target - written) as usize;
    if remainder > 0 {
        file.write_all(&chunk[..remainder]).unwrap();
    }
    drop(file);

    zdm()
        .arg(&huge_path)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("0001_huge.py"))
        // The error from `Error::FileTooLarge` is "File too large:
        // <path> (<size> bytes, max <max> bytes)". Pin the prefix so
        // a renaming-only refactor (or losing the size check
        // entirely) shows up here instead of slipping through as a
        // generic IO error.
        .stderr(predicate::str::contains("File too large"));
}

#[test]
fn e2e_r015_warns_alter_field_not_null() {
    // R015 surfaces nullable→NOT NULL transitions as a Warning rather
    // than an Error, because the rule cannot tell a genuine transition
    // from a benign AlterField on an already-NOT-NULL column. The
    // diagnostic appears in stdout but does not fail the build.
    let fixture = fixtures_dir().join("R015/fail_alter_field_not_null.py");

    zdm()
        .arg(&fixture)
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("R015"));
}

// =============================================================================
// Multiple Rules in Same File
// =============================================================================

#[test]
fn e2e_multiple_rules_detect_all() {
    // Lint multiple failing fixtures at once
    let r001_fail = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");
    let r016_fail = fixtures_dir().join("R016/fail_remove_index_non_concurrent.py");

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
    // Self-confirming-pass risk: a bare `.success().code(0)`
    // would pass if R001 silently stopped firing on this
    // fixture (registry dark, op-extractor broken). Pair with
    // a control invocation that proves R001 fires WITHOUT
    // `--ignore`, so the test catches "rule stopped working"
    // as well as "rule was incorrectly suppressed".
    let fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");

    // Control: rule fires when not ignored.
    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R001"));

    // Subject: rule is suppressed when --ignore is set.
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
    // Same pass-only risk as e2e_ignore_rule_skips_detection.
    // Pair with a control that proves --select R001 still fires
    // on this fixture, so a silent registry regression can't
    // pass the test by emitting nothing.
    let fixture = fixtures_dir().join("R001/fail_non_concurrent_add_index.py");

    // Control: --select R001 fires.
    zdm()
        .arg(&fixture)
        .arg("--select")
        .arg("R001")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R001"));

    // Subject: --select R002 produces no diagnostic for this R001 fixture.
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
    // Scan a Django-style app/migrations layout containing one safe and
    // one unsafe migration. Directory discovery only descends into
    // `migrations/` subdirectories, which is why the per-rule fixture dirs
    // (tests/fixtures/rules/R*/) can't be used here directly.
    let scan_root = fixtures_root().join("scan");

    let output = zdm()
        .arg(scan_root)
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stdout
        .clone();

    let output_str = String::from_utf8(output).unwrap();

    assert!(
        output_str.contains("R001"),
        "expected R001 violation in output, got: {output_str}"
    );
    assert!(
        output_str.contains("0002_unsafe_index.py"),
        "expected unsafe fixture in output, got: {output_str}"
    );
    assert!(
        !output_str.contains("0001_initial.py"),
        "safe fixture should not be reported, got: {output_str}"
    );
}

#[cfg(unix)]
#[test]
fn e2e_hostile_filename_with_newline_does_not_inject_fake_diagnostics() {
    // Pin the end-to-end injection-defense the sanitizer split
    // exists for: a migration filename containing a literal `\n`
    // would otherwise produce stderr output like
    //
    //   error: <real msg> for path
    //     --> /repo/app/migrations/legit.py:1
    //     --> /etc/passwd:1
    //     ...
    //
    // because every output sink interpolates the path. The
    // sanitizer must escape the embedded `\n` so the injected
    // line shows up as `\x0a` and the user sees one logical line.
    use std::fs;

    let temp = tempfile::TempDir::new().unwrap();
    // Plant `.git` so the config walk-up doesn't escape into the
    // host's pyproject.
    fs::create_dir_all(temp.path().join(".git")).unwrap();
    let migrations_dir = temp.path().join("app").join("migrations");
    fs::create_dir_all(&migrations_dir).unwrap();
    fs::write(migrations_dir.join("__init__.py"), "").unwrap();

    // Write content that will trigger R001 to ensure the path
    // appears in a diagnostic.
    let trigger = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;
    let hostile_path = migrations_dir.join("0001_hostile\n  --> /etc/passwd:1\nfake.py");
    let write_result = fs::write(&hostile_path, trigger);
    if let Err(e) = write_result {
        // Some filesystems (notably ZFS with utf8only=on, and
        // a handful of sandboxed CI runners) reject control
        // characters in filenames. The test is meaningful only
        // when the OS lets us create the hostile filename — emit
        // a marker so CI logs distinguish "skipped" from "passed".
        // Without this, a regression that breaks the sanitizer
        // would go unnoticed if every CI runner happened to land
        // on a hostile-rejecting filesystem.
        eprintln!("e2e_hostile_filename: filesystem rejected hostile filename ({e}), skipping",);
        return;
    }

    let output = zdm().arg(temp.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("\\x0a"),
        "expected literal `\\x0a` (escaped LF) in output to prove the hostile filename was sanitized, got:\n{stdout}",
    );
    assert!(
        !stdout.contains("\n  --> /etc/passwd:1\n"),
        "the injected `--> /etc/passwd:1` line must not appear unsanitized in output, got:\n{stdout}",
    );
}

#[cfg(unix)]
#[test]
fn e2e_explicit_symlink_path_is_rejected_not_followed() {
    // The discovery WALK rejects symlinked entries via
    // `symlink_metadata` (since commit 6299212). The
    // explicitly-passed-file branch in main.rs used to call
    // `path.is_file()` which transparently follows symlinks, so a
    // symlink to e.g. `/etc/passwd` could still reach the parser.
    // This e2e pins that the binary now applies the same
    // rejection on the explicit branch — the source under the
    // symlink target must not appear in output, and the binary
    // must not crash by trying to parse it as Python.
    use std::fs;
    use std::os::unix::fs::symlink;

    let temp = tempfile::TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join(".git")).unwrap();
    let migrations_dir = temp.path().join("app").join("migrations");
    fs::create_dir_all(&migrations_dir).unwrap();
    fs::write(migrations_dir.join("__init__.py"), "").unwrap();

    let real_target = temp.path().join("secret_real.py");
    let real_content = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;
    fs::write(&real_target, real_content).unwrap();

    let symlink_path = migrations_dir.join("0001_link.py");
    symlink(&real_target, &symlink_path).unwrap();

    // Pass the symlink path explicitly. Without the fix, the
    // explicit-path branch would follow the link, parse the
    // target, and emit R001 against the symlink path.
    let output = zdm().arg(&symlink_path).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stdout.contains("R001") && !stderr.contains("R001"),
        "explicit symlink path should be skipped, not followed and linted; \
         got stdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

// =============================================================================
// E2E coverage for rules that previously had no fixture-based test.
//
// R004, R005, R013, R017 each shipped with only in-rule unit tests.
// A regression in the output sink, operation extractor, or registry
// assembly could silently break end-to-end emission for these
// rules without any integration test failing. Add minimal fail
// fixtures + e2e tests so the binary's plumbing is exercised for
// every rule that ships diagnostics from a single-file lint.
//
// R009 is deliberately skipped here — it's a changeset rule that
// only fires across multiple files in a diff, so its meaningful
// e2e coverage lives in tests/cli_integration.rs's diff-mode
// section rather than a single-file fixture invocation.
// =============================================================================

#[test]
fn e2e_r004_fail_concurrent_without_atomic_false() {
    let fixture = fixtures_dir().join("R004/fail_concurrent_without_atomic_false.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R004"));
}

#[test]
fn e2e_r005_fail_remove_field_without_separate() {
    let fixture = fixtures_dir().join("R005/fail_remove_field_without_separate.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R005"));
}

#[test]
fn e2e_r013_fail_irreversible_run_sql() {
    // R013 is a Warning, so exit code stays 0 unless we promote
    // it. Pin both the exit code AND the rule ID in stdout so a
    // silently-suppressed warning doesn't pass the test.
    let fixture = fixtures_dir().join("R013/fail_irreversible_run_sql.py");

    zdm()
        .arg(&fixture)
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("R013"))
        .stdout(predicate::str::contains("warning"));
}

#[test]
fn e2e_r017_fail_check_constraint() {
    let fixture = fixtures_dir().join("R017/fail_check_constraint.py");

    zdm()
        .arg(&fixture)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("R017"));
}
