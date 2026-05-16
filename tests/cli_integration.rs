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

/// Helper to create a temp directory with migration files
fn setup_migrations(migrations: &[(&str, &str)]) -> TempDir {
    let temp_dir = TempDir::new().unwrap();
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
        let temp = setup_migrations(&[("0001_warning.py", WARNING_ONLY_MIGRATION)]);

        zdm().arg(temp.path()).assert().success().code(0);
    }

    #[test]
    fn exit_1_when_warnings_as_errors() {
        let temp = setup_migrations(&[("0001_warning.py", WARNING_ONLY_MIGRATION)]);

        zdm()
            .arg(temp.path())
            .arg("--warnings-as-errors")
            .assert()
            .failure()
            .code(1);
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
        let temp = TempDir::new().unwrap();

        zdm().arg(temp.path()).assert().success().code(0);
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
        let temp = setup_git_repo();
        let migration_path = temp
            .path()
            .join("app")
            .join("migrations")
            .join("0001_initial.py");
        fs::write(&migration_path, CLEAN_MIGRATION).unwrap();
        git_stage(temp.path(), "app/migrations/0001_initial.py");
        fs::write(&migration_path, BAD_MIGRATION_NON_CONCURRENT_INDEX).unwrap();

        zdm()
            .current_dir(temp.path())
            .arg("--diff-staged")
            .arg("origin/main")
            .arg("--select")
            .arg("R001")
            .assert()
            .success()
            .code(0);
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

        // A staged migration that lives under an excluded path. Without the
        // exclude filter being applied to staged discovery, R001 would fire.
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

        zdm()
            .current_dir(temp.path())
            .arg("--diff-staged")
            .arg("origin/main")
            .arg("--select")
            .arg("R001")
            .assert()
            .success()
            .code(0);
    }

    #[test]
    fn r008_allows_basename_patterns() {
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
        fs::write(model_dir.join("models.py"), "# model").unwrap();

        git_commit_all(temp.path(), "Add migration and model");

        zdm()
            .current_dir(temp.path())
            .arg("--diff")
            .arg("origin/main")
            .arg("--select")
            .arg("R008")
            .assert()
            .success()
            .code(0);
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
    fn json_output_is_valid_json() {
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
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert!(parsed.is_object() || parsed.is_array());
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

        // Should contain rule_id, message, path, severity
        assert!(json_str.contains("\"rule_id\""));
        assert!(json_str.contains("\"message\""));
        assert!(json_str.contains("\"path\""));
        assert!(json_str.contains("\"severity\""));
    }

    #[test]
    fn compact_output_one_line_per_diagnostic() {
        let temp = setup_migrations(&[("0001_bad.py", BAD_MIGRATION)]);

        zdm()
            .arg(temp.path())
            .arg("--output-format")
            .arg("compact")
            .assert()
            .failure()
            // Compact format: `path:line: SEV: [RULE_ID rule-name] message`.
            .stdout(predicate::str::contains(
                "E: [R001 non-concurrent-add-index]",
            ));
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

        // With --select R001, should only see R001, not R016
        zdm()
            .arg(temp.path())
            .arg("--select")
            .arg("R001")
            .assert()
            .failure()
            .stdout(predicate::str::contains("R001"))
            .stdout(predicate::str::contains("R016").not());
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

        zdm()
            .arg(temp.path())
            .arg("--select")
            .arg("R001,R016")
            .assert()
            .failure()
            .stdout(predicate::str::contains("R001"))
            .stdout(predicate::str::contains("R016"));
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
