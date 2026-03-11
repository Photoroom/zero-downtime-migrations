//! CLI integration tests
//!
//! Tests the actual CLI binary with various flag combinations,
//! verifying exit codes and output formats.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Helper to create a command for the `zdm` binary
fn zdm() -> Command {
    Command::cargo_bin("zdm").unwrap()
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

// =============================================================================
// Exit Code Tests
// =============================================================================

mod exit_codes {
    use super::*;

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

    const WARNING_ONLY_MIGRATION: &str = r#"
from django.db import migrations, models

class Migration(migrations.Migration):
    dependencies = []
    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
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
            .stdout(predicate::str::contains("E: R001"));
    }
}

// =============================================================================
// Rule Selection Tests
// =============================================================================

mod rule_selection {
    use super::*;

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
            .stderr(predicate::str::contains("Unknown rule"));
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
