//! CLI integration tests
//!
//! Tests the actual CLI binary with various flag combinations,
//! verifying exit codes and output formats.

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::process::Command as StdCommand;
use tempfile::TempDir;

/// Helper to create a command for the `zdm` binary
fn zdm() -> Command {
    cargo_bin_cmd!("zdm")
}

/// Helper to create a temp directory with migration files.
///
/// Plants an empty `.git` directory at the temp root so the config
/// loader's walk-up doesn't escape into the actual project's
/// pyproject.toml (which would silently change `[tool.zdm]` settings
/// under the tests). The walk-up is guarded by a `.git` ancestor
/// requirement, and the sentinel keeps it scoped to our temp root.
fn setup_migrations(migrations: &[(&str, &str)]) -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    fs::create_dir_all(temp_dir.path().join(".git")).unwrap();
    let migrations_dir = temp_dir.path().join("app").join("migrations");
    fs::create_dir_all(&migrations_dir).unwrap();

    // Create __init__.py
    fs::write(migrations_dir.join("__init__.py"), "").unwrap();

    for (name, content) in migrations {
        fs::write(migrations_dir.join(name), content).unwrap();
    }

    temp_dir
}

fn git_init(path: &std::path::Path) {
    StdCommand::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .expect("Failed to init git repo");
    StdCommand::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(path)
        .output()
        .expect("Failed to set git user email");
    StdCommand::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .expect("Failed to set git user name");
}

fn git_commit_all(path: &std::path::Path, message: &str) {
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .expect("Failed to stage files");
    StdCommand::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .output()
        .expect("Failed to commit files");
}

fn git_stage(path: &std::path::Path, file: &str) {
    StdCommand::new("git")
        .args(["add", file])
        .current_dir(path)
        .output()
        .expect("Failed to stage file");
}

const CLEAN_MIGRATION: &str = r#"
from django.db import migrations

class Migration(migrations.Migration):
    dependencies = []
    operations = []
"#;

const BAD_MIGRATION_NON_CONCURRENT_INDEX: &str = r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    dependencies = []
    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

// =============================================================================
// Exit Code Tests
// =============================================================================

mod exit_codes {
    use super::*;

    const WARNING_ONLY_MIGRATION: &str = r#"
from django.db import migrations

def forwards(apps, schema_editor):
    pass

class Migration(migrations.Migration):
    dependencies = []
    operations = [
        migrations.RunPython(forwards),
    ]
"#;

    #[test]
    fn exit_0_when_no_issues() {
        let temp = setup_migrations(&[("0001_initial.py", CLEAN_MIGRATION)]);

        zdm().arg(temp.path()).assert().success().code(0);
    }

    #[test]
    fn exit_1_when_errors_found() {
        let temp = setup_migrations(&[("0001_bad.py", BAD_MIGRATION_NON_CONCURRENT_INDEX)]);

        zdm().arg(temp.path()).assert().failure().code(1);
    }

    #[test]
    fn exit_0_when_only_warnings() {
        // Exit-0 alone is self-confirming: if every rule silently
        // stopped emitting diagnostics (so `WARNING_ONLY_MIGRATION`
        // produced nothing), the test would still pass and we'd
        // never notice. Pin two facts: the binary exits 0, AND R012
        // (the irreversible-RunPython warning targeted by the
        // fixture) actually surfaces in stdout.
        let temp = setup_migrations(&[("0001_warning.py", WARNING_ONLY_MIGRATION)]);

        zdm()
            .arg(temp.path())
            .assert()
            .success()
            .code(0)
            .stdout(predicate::str::contains("R012"))
            .stdout(predicate::str::contains("warning"));
    }

    #[test]
    fn exit_1_when_warnings_as_errors() {
        // Pin R012 in stdout so the test catches "warnings-as-
        // errors promoted SOMETHING to error" rather than just
        // "some error fired." Without this, a future bug that
        // makes the fixture trip a different rule (e.g. R001 on
        // an unrelated change) would still pass `.code(1)`
        // without proving the warning-promotion path itself
        // works. Mirrors `exit_0_when_only_warnings` which
        // already pins R012.
        let temp = setup_migrations(&[("0001_warning.py", WARNING_ONLY_MIGRATION)]);

        zdm()
            .arg(temp.path())
            .arg("--warnings-as-errors")
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("R012"));
    }

    #[test]
    fn exit_2_when_invalid_path() {
        zdm()
            .arg("/nonexistent/path/that/does/not/exist")
            .assert()
            .failure()
            .code(2);
    }

    #[test]
    fn exit_0_when_no_migrations_found() {
        // Plant `.git` so the config walk-up doesn't escape into
        // the host's pyproject (the test would still pass either
        // way because zero migrations always exits 0, but pinning
        // the boundary matches every other CLI test in this file).
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".git")).unwrap();

        // Pin "no migrations" produces empty stdout — a future
        // change that surfaces an "scanned 0 files" line would
        // change observable behaviour and should fail this test.
        zdm()
            .arg(temp.path())
            .assert()
            .success()
            .code(0)
            .stdout(predicate::str::is_empty());
    }
}

// =============================================================================
// Diff Mode Tests
// =============================================================================

mod diff_mode {
    use super::*;

    fn setup_git_repo() -> TempDir {
        let temp = TempDir::new().unwrap();
        git_init(temp.path());

        let migrations_dir = temp.path().join("app").join("migrations");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(migrations_dir.join("__init__.py"), "").unwrap();
        fs::write(temp.path().join("README.md"), "# Test").unwrap();

        git_commit_all(temp.path(), "Initial");

        StdCommand::new("git")
            .args(["branch", "origin/main"])
            .current_dir(temp.path())
            .output()
            .expect("Failed to create base branch");

        temp
    }

    #[test]
    fn diff_staged_detects_staged_migration_not_in_head() {
        let temp = setup_git_repo();
        let migration_path = temp
            .path()
            .join("app")
            .join("migrations")
            .join("0001_bad.py");
        fs::write(&migration_path, BAD_MIGRATION_NON_CONCURRENT_INDEX).unwrap();
        git_stage(temp.path(), "app/migrations/0001_bad.py");

        zdm()
            .current_dir(temp.path())
            .arg("--diff-staged")
            .arg("origin/main")
            .arg("--select")
            .arg("R001")
            .arg("--output-format")
            .arg("compact")
            .assert()
            .failure()
            .stdout(predicate::str::contains("R001"));
    }

    #[test]
    fn diff_staged_reads_index_content_not_worktree_content() {
        // The original test was self-confirming: a bare
        // `.success().code(0)` would pass if the diff-staged
        // pipeline went dark (no files extracted, no rules run,
        // no diagnostics emitted). Stage TWO files: one whose
        // index-content is CLEAN but worktree-content is BAD
        // (the subject — proves we read the index, not the
        // worktree, so no diagnostic), and one whose
        // index-content is BAD (the control — proves the
        // pipeline is alive and would surface a real index
        // violation). If the diff-staged plumbing silently
        // emitted nothing, the control assertion would fail.
        let temp = setup_git_repo();
        let migrations_dir = temp.path().join("app").join("migrations");

        // Subject: index = clean, worktree = bad. No diagnostic.
        let subject_path = migrations_dir.join("0001_initial.py");
        fs::write(&subject_path, CLEAN_MIGRATION).unwrap();
        git_stage(temp.path(), "app/migrations/0001_initial.py");
        fs::write(&subject_path, BAD_MIGRATION_NON_CONCURRENT_INDEX).unwrap();

        // Control: index = bad, worktree = bad (same file). Diagnostic.
        let control_path = migrations_dir.join("0002_control_bad.py");
        fs::write(&control_path, BAD_MIGRATION_NON_CONCURRENT_INDEX).unwrap();
        git_stage(temp.path(), "app/migrations/0002_control_bad.py");

        zdm()
            .current_dir(temp.path())
            .arg("--diff-staged")
            .arg("origin/main")
            .arg("--select")
            .arg("R001")
            .arg("--output-format")
            .arg("compact")
            .assert()
            .failure()
            .code(1)
            // The control fires: the diff plumbing is alive.
            .stdout(predicate::str::contains("0002_control_bad.py"))
            // The subject does not fire: we read the index, not
            // the worktree, so the clean staged content wins.
            .stdout(predicate::str::contains("0001_initial.py").not());
    }

    #[test]
    fn diff_staged_respects_exclude_patterns() {
        let temp = setup_git_repo();

        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"exclude = ["**/test_migrations/**"]"#,
        )
        .unwrap();
        git_commit_all(temp.path(), "Add config");

        StdCommand::new("git")
            .args(["branch", "-f", "origin/main", "HEAD"])
            .current_dir(temp.path())
            .output()
            .expect("Failed to update base branch");

        // Staged migration A lives under the excluded path; without
        // the exclude filter R001 would fire on it.
        let excluded_dir = temp
            .path()
            .join("app")
            .join("test_migrations")
            .join("migrations");
        fs::create_dir_all(&excluded_dir).unwrap();
        fs::write(excluded_dir.join("__init__.py"), "").unwrap();
        fs::write(
            excluded_dir.join("0001_bad.py"),
            BAD_MIGRATION_NON_CONCURRENT_INDEX,
        )
        .unwrap();
        git_stage(temp.path(), "app/test_migrations");

        // The exclude-only assertion was self-confirming: if the diff
        // machinery silently missed staged files, the test would
        // still pass with exit 0. Stage a CONTROL migration in a
        // NON-excluded path with the same bad content — that file
        // must fire R001, proving the exclude is what suppressed the
        // first one rather than the discovery layer ignoring
        // everything.
        let control_dir = temp.path().join("app").join("migrations");
        fs::write(
            control_dir.join("0002_control_bad.py"),
            BAD_MIGRATION_NON_CONCURRENT_INDEX,
        )
        .unwrap();
        git_stage(temp.path(), "app/migrations/0002_control_bad.py");

        let output = zdm()
            .current_dir(temp.path())
            .arg("--diff-staged")
            .arg("origin/main")
            .arg("--select")
            .arg("R001")
            .arg("--output-format")
            .arg("json")
            .assert()
            .failure()
            .code(1)
            .get_output()
            .stdout
            .clone();
        let json: serde_json::Value =
            serde_json::from_str(&String::from_utf8(output).unwrap()).unwrap();
        let diags = json
            .get("diagnostics")
            .and_then(|v| v.as_array())
            .expect("JSON must include a `diagnostics` array");
        // Exactly one diagnostic: the control. The excluded file
        // contributes nothing.
        assert_eq!(
            diags.len(),
            1,
            "expected only the control file to fire, got: {diags:#?}",
        );
        let path = diags[0]["path"].as_str().unwrap();
        assert!(
            path.ends_with("0002_control_bad.py"),
            "the surviving diagnostic must be the control, got path: {path}",
        );
        assert!(
            !path.contains("test_migrations"),
            "an excluded-path diagnostic should not surface, got: {path}",
        );
    }

    #[test]
    fn r008_allows_basename_patterns() {
        // This is currently the ONLY R008 coverage outside unit
        // tests, and the bare `.success().code(0)` would pass if
        // R008 silently stopped firing in diff mode entirely.
        // Add a second changed file (`views.py`) that does NOT
        // match the allowed-file-patterns — R008 must fire on it,
        // proving the rule is alive on this branch.
        let temp = setup_git_repo();

        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"allowed-file-patterns = ["models.py"]"#,
        )
        .unwrap();
        git_commit_all(temp.path(), "Add config");

        StdCommand::new("git")
            .args(["branch", "-f", "origin/main", "HEAD"])
            .current_dir(temp.path())
            .output()
            .expect("Failed to update base branch");

        let migration_path = temp
            .path()
            .join("app")
            .join("migrations")
            .join("0001_initial.py");
        fs::write(&migration_path, CLEAN_MIGRATION).unwrap();

        let model_dir = temp.path().join("backend").join("media");
        fs::create_dir_all(&model_dir).unwrap();
        // Allowed: matches the `models.py` basename pattern.
        fs::write(model_dir.join("models.py"), "# model").unwrap();
        // Not allowed: matches no pattern. R008 should fire on this.
        fs::write(model_dir.join("views.py"), "# view").unwrap();

        git_commit_all(temp.path(), "Add migration, model, and view");

        zdm()
            .current_dir(temp.path())
            .arg("--diff")
            .arg("origin/main")
            .arg("--select")
            .arg("R008")
            .assert()
            .failure()
            .code(1)
            // The not-allowed file fires.
            .stdout(predicate::str::contains("R008"))
            .stdout(predicate::str::contains("views.py"))
            // The allowed-basename file does not.
            .stdout(predicate::str::contains("models.py").not());
    }
}

// =============================================================================
// Output Format Tests
// =============================================================================

mod output_format {
    use super::*;

    const BAD_MIGRATION: &str = r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    dependencies = []
    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn default_output_shows_filename_and_rule() {
        let temp = setup_migrations(&[("0001_bad.py", BAD_MIGRATION)]);

        zdm()
            .arg(temp.path())
            .assert()
            .failure()
            .stdout(predicate::str::contains("R001"))
            .stdout(predicate::str::contains("0001_bad.py"));
    }

    #[test]
    fn json_output_contains_required_fields() {
        let temp = setup_migrations(&[("0001_bad.py", BAD_MIGRATION)]);

        let output = zdm()
            .arg(temp.path())
            .arg("--output-format")
            .arg("json")
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone();

        let json_str = String::from_utf8(output).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("output must be valid JSON");

        // Parse structurally rather than grepping the raw text: an
        // accidental rename of `path` to `pathname` (or moving fields
        // into a nested object) would otherwise still pass the old
        // substring check.
        let diagnostics = parsed
            .get("diagnostics")
            .and_then(|v| v.as_array())
            .expect("top-level JSON must have a `diagnostics` array");
        assert!(
            !diagnostics.is_empty(),
            "expected at least one diagnostic in the JSON output"
        );
        let diag = diagnostics[0]
            .as_object()
            .expect("each diagnostic must be a JSON object");
        // The full documented per-diagnostic shape: a rename or
        // accidental drop of any of these would otherwise slip
        // through the substring grep that preceded this commit.
        for field in [
            "rule_id",
            "rule_name",
            "message",
            "path",
            "severity",
            "line",
            "column",
            "help",
        ] {
            assert!(
                diag.contains_key(field),
                "diagnostic is missing required field `{field}`, got: {diag:?}",
            );
        }
        assert_eq!(diag["rule_id"].as_str(), Some("R001"));
        assert_eq!(diag["severity"].as_str(), Some("error"));
        assert!(
            diag["path"]
                .as_str()
                .is_some_and(|s| s.ends_with("0001_bad.py")),
            "path should end with the fixture filename, got: {:?}",
            diag["path"],
        );

        // The documented JSON shape also carries a `summary` object.
        let summary = parsed
            .get("summary")
            .and_then(|v| v.as_object())
            .expect("top-level JSON must include a `summary` object");
        assert_eq!(summary["total"].as_u64(), Some(diagnostics.len() as u64));
        assert!(summary["errors"].as_u64().unwrap() >= 1);
        assert!(
            summary.contains_key("warnings"),
            "summary must carry a `warnings` count even when zero"
        );
    }

    #[test]
    fn json_output_for_clean_run_contains_empty_summary() {
        let temp = setup_migrations(&[("0001_clean.py", CLEAN_MIGRATION)]);

        let output = zdm()
            .arg(temp.path())
            .arg("--output-format")
            .arg("json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();

        let parsed: serde_json::Value =
            serde_json::from_slice(&output).expect("clean JSON output must be valid JSON");
        assert_eq!(parsed["diagnostics"].as_array().unwrap().len(), 0);
        assert_eq!(parsed["summary"]["total"].as_u64(), Some(0));
        assert_eq!(parsed["summary"]["errors"].as_u64(), Some(0));
        assert_eq!(parsed["summary"]["warnings"].as_u64(), Some(0));
    }

    #[test]
    fn compact_output_one_line_per_diagnostic() {
        let temp = setup_migrations(&[("0001_bad.py", BAD_MIGRATION)]);

        let output = zdm()
            .arg(temp.path())
            .arg("--output-format")
            .arg("compact")
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(output).unwrap();

        // Compact format prints one `path:line: SEV: [RULE_ID
        // rule-name] message` line per diagnostic, plus an indented
        // `  help: ...` continuation line when the rule supplies help
        // text. R001 supplies help, so we expect a diagnostic line +
        // a help line — but no third line.
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "expected exactly two compact-format lines (diag + help), got {lines:#?}",
        );
        let diag_line = lines[0];
        assert!(
            diag_line.contains("0001_bad.py"),
            "compact line should start with the path, got: {diag_line}",
        );
        assert!(
            diag_line.contains("E: [R001 non-concurrent-add-index]"),
            "compact line should carry SEV: [RULE rulename], got: {diag_line}",
        );
        // The compact line shape is
        //   <path>:<line>:<col>: <SEV>: [<RULE> <name>] <msg>
        // and we want to assert <line> is a number. On Windows the
        // path itself contains a colon (drive letter, e.g.
        // `C:\Users\...`), so a naive `diag_line.find(':')` lands
        // on the drive-letter colon and the next field is the
        // rest of the path. Find the path/line boundary by
        // searching for `.py:` instead — every migration fixture
        // is a `.py` file, so that anchor is unambiguous.
        let py_marker = ".py:";
        let py_idx = diag_line
            .find(py_marker)
            .expect("compact line must contain a .py path followed by :");
        let after_path = &diag_line[py_idx + py_marker.len()..];
        let next_colon = after_path
            .find(':')
            .expect("compact line must have a `:line:` after the path");
        let line_field = &after_path[..next_colon];
        assert!(
            line_field.parse::<usize>().is_ok(),
            "expected a line-number field after the path, got: {line_field:?}",
        );

        // Help continuation is two-space indented and starts with `help:`.
        assert!(
            lines[1].starts_with("  help:"),
            "help continuation must start with `  help:`, got: {}",
            lines[1],
        );
    }
}

// =============================================================================
// Rule Selection Tests
// =============================================================================

mod rule_selection {
    use super::*;

    // This fixture triggers exactly R001 (non-concurrent AddIndex) and
    // R016 (non-concurrent RemoveIndex). Tests in this module rely on
    // those being the only diagnostics emitted.
    const MIGRATION_WITH_MULTIPLE_ISSUES: &str = r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    dependencies = []
    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
        migrations.RemoveIndex(
            model_name='product',
            name='old_idx',
        ),
    ]
"#;

    #[test]
    fn select_only_runs_specified_rules() {
        let temp = setup_migrations(&[("0001_multi.py", MIGRATION_WITH_MULTIPLE_ISSUES)]);

        // With --select R001, the JSON output should contain exactly
        // one diagnostic (the R001 hit). The previous substring-only
        // check would have missed a regression that added a third
        // rule's diagnostic (e.g. R005) since "R016" is the only
        // negative check.
        let output = zdm()
            .arg(temp.path())
            .arg("--select")
            .arg("R001")
            .arg("--output-format")
            .arg("json")
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone();
        let json: serde_json::Value =
            serde_json::from_str(&String::from_utf8(output).unwrap()).unwrap();
        let diags = json
            .get("diagnostics")
            .and_then(|v| v.as_array())
            .expect("JSON output must include a `diagnostics` array");
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one diagnostic with --select R001, got: {diags:#?}",
        );
        assert_eq!(diags[0]["rule_id"].as_str(), Some("R001"));
    }

    #[test]
    fn ignore_skips_specified_rules() {
        let temp = setup_migrations(&[("0001_multi.py", MIGRATION_WITH_MULTIPLE_ISSUES)]);

        // With --ignore R001, should not see R001 but should see R016
        zdm()
            .arg(temp.path())
            .arg("--ignore")
            .arg("R001")
            .assert()
            .failure()
            .stdout(predicate::str::contains("R001").not())
            .stdout(predicate::str::contains("R016"));
    }

    #[test]
    fn select_multiple_rules() {
        let temp = setup_migrations(&[("0001_multi.py", MIGRATION_WITH_MULTIPLE_ISSUES)]);

        // Substring-only assertions would silently pass if a
        // third rule erroneously fired alongside R001 and R016;
        // pin the count via JSON.
        let output = zdm()
            .arg(temp.path())
            .arg("--select")
            .arg("R001,R016")
            .arg("--output-format")
            .arg("json")
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let ids: Vec<&str> = json["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["rule_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"R001"));
        assert!(ids.contains(&"R016"));
        assert!(
            ids.iter().all(|id| matches!(*id, "R001" | "R016")),
            "no rule other than R001/R016 should fire, got {ids:?}",
        );
    }

    #[test]
    fn ignore_all_violations_results_in_exit_0() {
        let temp = setup_migrations(&[("0001_multi.py", MIGRATION_WITH_MULTIPLE_ISSUES)]);

        zdm()
            .arg(temp.path())
            .arg("--ignore")
            .arg("R001,R016")
            .assert()
            .success()
            .code(0);
    }

    #[test]
    fn cli_ignore_is_additive_to_config_ignore() {
        let temp = setup_migrations(&[("0001_multi.py", MIGRATION_WITH_MULTIPLE_ISSUES)]);
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R016"]"#,
        )
        .unwrap();

        // Config ignores R016, CLI adds R001. Both should be suppressed and
        // the run should exit clean.
        zdm()
            .current_dir(temp.path())
            .arg("--ignore")
            .arg("R001")
            .assert()
            .success()
            .code(0);
    }

    #[test]
    fn cli_select_replaces_config_select() {
        let temp = setup_migrations(&[("0001_multi.py", MIGRATION_WITH_MULTIPLE_ISSUES)]);
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"select = ["R001"]"#,
        )
        .unwrap();

        // Config selects R001 only; CLI overrides with R016 only. R001 must
        // not be reported; R016 must.
        zdm()
            .current_dir(temp.path())
            .arg("--select")
            .arg("R016")
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("R001").not())
            .stdout(predicate::str::contains("R016"));
    }

    const MIGRATION_WITH_IGNORE_COMMENTS: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddIndex(  # zdm: ignore R001
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
        migrations.RemoveIndex(
            model_name='product',
            name='old_idx',
        ),
    ]
"#;

    #[test]
    fn inline_ignore_directive_suppresses_diagnostic() {
        // The AddIndex carries `# zdm: ignore R001`, so R001 should be
        // silent on that operation. RemoveIndex has no directive, so
        // R016 still fires and the run exits non-zero.
        let temp = setup_migrations(&[("0001.py", MIGRATION_WITH_IGNORE_COMMENTS)]);
        zdm()
            .arg(temp.path())
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("R001").not())
            .stdout(predicate::str::contains("R016"));
    }

    const MIGRATION_WITH_FULL_IGNORE: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        # zdm: ignore R001, R016
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
        migrations.RemoveIndex(  # zdm: ignore R016
            model_name='product',
            name='old_idx',
        ),
    ]
"#;

    #[test]
    fn inline_ignore_directive_above_and_same_line_both_work() {
        // R001 is suppressed by a comment on the line above AddIndex;
        // R016 is suppressed by a same-line comment on RemoveIndex.
        // Nothing else fires, so the run exits clean.
        let temp = setup_migrations(&[("0001.py", MIGRATION_WITH_FULL_IGNORE)]);
        zdm().arg(temp.path()).assert().success().code(0);
    }

    const MIGRATION_WITH_NON_MATCHING_IGNORE: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        # zdm: ignore R015
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn inline_ignore_directive_for_different_rule_does_not_suppress() {
        // The user wrote `# zdm: ignore R015`, but R001 is what actually
        // fires on AddIndex. R001 must still be reported.
        let temp = setup_migrations(&[("0001.py", MIGRATION_WITH_NON_MATCHING_IGNORE)]);
        zdm()
            .arg(temp.path())
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("R001"));
    }
}

// =============================================================================
// Rule Command Tests
// =============================================================================

mod rule_command {
    use super::*;

    #[test]
    fn rule_command_shows_description() {
        zdm()
            .arg("rule")
            .arg("R001")
            .assert()
            .success()
            .stdout(predicate::str::contains("R001"))
            .stdout(predicate::str::contains("AddIndex"));
    }

    #[test]
    fn rule_command_unknown_rule_fails() {
        zdm()
            .arg("rule")
            .arg("R999")
            .assert()
            .failure()
            .code(2)
            .stderr(predicate::str::contains("Unknown rule"))
            // Pin the no-duplicate behaviour: the previous implementation
            // printed "error: Unknown rule: R999" twice (once via an
            // explicit `eprintln!` in `run_rule_command`, once via
            // `main()`'s error sink).
            .stderr(predicate::function(|s: &str| {
                s.matches("Unknown rule").count() == 1
            }))
            // The error now lists the available rule IDs so the user
            // gets actionable feedback (e.g. "did you mean R001?").
            .stderr(predicate::str::contains("available rules:"))
            .stderr(predicate::str::contains("R001"));
    }
}

// =============================================================================
// Multiple Files Tests
// =============================================================================

mod multiple_files {
    use super::*;

    const CLEAN_MIGRATION: &str = r#"
from django.db import migrations

class Migration(migrations.Migration):
    dependencies = []
    operations = []
"#;

    const BAD_MIGRATION: &str = r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    dependencies = []
    operations = [
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['name'], name='product_name_idx'),
        ),
    ]
"#;

    #[test]
    fn lint_multiple_files_in_directory() {
        let temp = setup_migrations(&[
            ("0001_initial.py", CLEAN_MIGRATION),
            ("0002_bad.py", BAD_MIGRATION),
        ]);

        zdm()
            .arg(temp.path())
            .assert()
            .failure()
            .stdout(predicate::str::contains("0002_bad.py"))
            .stdout(predicate::str::contains("R001"));
    }

    #[test]
    fn lint_specific_file() {
        let temp = setup_migrations(&[
            ("0001_initial.py", CLEAN_MIGRATION),
            ("0002_bad.py", BAD_MIGRATION),
        ]);

        let migrations_dir = temp.path().join("app").join("migrations");

        // Only lint the clean file
        zdm()
            .arg(migrations_dir.join("0001_initial.py"))
            .assert()
            .success()
            .code(0);

        // Only lint the bad file
        zdm()
            .arg(migrations_dir.join("0002_bad.py"))
            .assert()
            .failure()
            .code(1);
    }

    #[test]
    fn lint_multiple_specific_files() {
        let temp = setup_migrations(&[
            ("0001_initial.py", CLEAN_MIGRATION),
            ("0002_bad.py", BAD_MIGRATION),
        ]);

        let migrations_dir = temp.path().join("app").join("migrations");

        zdm()
            .arg(migrations_dir.join("0001_initial.py"))
            .arg(migrations_dir.join("0002_bad.py"))
            .assert()
            .failure()
            .stdout(predicate::str::contains("0002_bad.py"));
    }
}

// =============================================================================
// Version and Help Tests
// =============================================================================

mod version_help {
    use super::*;

    #[test]
    fn version_flag() {
        zdm()
            .arg("--version")
            .assert()
            .success()
            .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn help_flag() {
        zdm()
            .arg("--help")
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "PostgreSQL migration safety linter",
            ));
    }
}

mod list_rules {
    use super::*;

    #[test]
    fn list_rules_prints_known_rules_and_exits_zero() {
        // `--list-rules` should short-circuit before config load
        // (so it works even outside a configured project) and print
        // every rule the binary knows about. The output uses each
        // rule's ID, severity, and name.
        zdm()
            .arg("--list-rules")
            .assert()
            .success()
            .code(0)
            .stdout(predicate::str::contains("R001"))
            .stdout(predicate::str::contains("non-concurrent-add-index"))
            .stdout(predicate::str::contains("R017"))
            // R015 was downgraded to Warning in this PR; the listing
            // should reflect the current severity.
            .stdout(predicate::str::contains("R015"))
            .stdout(predicate::str::contains("Warning"));
    }

    #[test]
    fn list_rules_includes_every_documented_rule() {
        // Drive the expected list from the actual registries (which
        // is what the binary walks) so the test can't desync from
        // the source of truth. The previous hardcoded array would
        // have silently passed if a contributor added a rule to the
        // registry but forgot to add it to the test list — and
        // would have silently failed if a rule was retired from the
        // registry but left in the array.
        use zero_downtime_migrations::rules::{ChangesetRuleRegistry, RuleRegistry};

        let mut expected_ids: Vec<&'static str> = RuleRegistry::new()
            .rules()
            .iter()
            .map(|r| r.id())
            .chain(ChangesetRuleRegistry::new().rules().iter().map(|r| r.id()))
            .collect();
        expected_ids.sort();
        expected_ids.dedup();
        assert!(
            !expected_ids.is_empty(),
            "registry returned zero rules — listing test is now self-confirming",
        );

        let output = zdm().arg("--list-rules").output().expect("zdm should run");
        assert!(
            output.status.success(),
            "--list-rules should exit 0, got status {:?}",
            output.status,
        );
        let stdout = String::from_utf8(output.stdout).expect("--list-rules output is UTF-8");
        for id in &expected_ids {
            assert!(
                stdout.contains(id),
                "--list-rules output is missing rule {id}; got:\n{stdout}",
            );
        }

        // Counter-assertion: the printed rule count should match
        // the expected total exactly. Catches the "extras" case
        // where stdout contains all expected IDs PLUS a stale one
        // (e.g. a retired R007 line still printed alongside live
        // rules).
        // Anchored shape: `R` followed by exactly three digits and
        // then whitespace. Catches the "wrap" case where
        // `--list-rules` later grows a multi-line format (e.g. an
        // indented help line that happens to start with `Rule …`),
        // which a `starts_with('R') && next-is-digit` filter would
        // over-count.
        let printed_count = stdout
            .lines()
            .filter(|l| {
                let trimmed = l.trim_start();
                let mut chars = trimmed.chars();
                if chars.next() != Some('R') {
                    return false;
                }
                let id_digits: String = chars.by_ref().take(3).collect();
                if id_digits.len() != 3 || !id_digits.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                // Next char after the 3-digit id must be whitespace
                // (separating the ID column from the name/severity
                // columns). This rejects accidental matches like
                // "R001-something" in body text.
                chars.next().is_some_and(char::is_whitespace)
            })
            .count();
        assert_eq!(
            printed_count,
            expected_ids.len(),
            "--list-rules printed {printed_count} rule lines, expected {} (one per registered rule). \
             Got:\n{stdout}",
            expected_ids.len(),
        );
    }
}
