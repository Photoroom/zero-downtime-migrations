//! Black-box contracts for R005's historical ORM evidence. Every repository is
//! real Git history: worktree contents must never substitute for HEAD/the index.
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

#[derive(Clone, Copy, Debug)]
enum Orm {
    SqlAlchemy,
    Tortoise,
}

impl Orm {
    fn name(self) -> &'static str {
        match self {
            Self::SqlAlchemy => "sqlalchemy",
            Self::Tortoise => "tortoise",
        }
    }

    fn model(self, field: &str) -> String {
        match self {
            Self::SqlAlchemy => format!(
                "from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\n\nclass Base(DeclarativeBase):\n    pass\n\nclass Job(Base):\n    __tablename__ = 'jobs'\n    id: Mapped[int] = mapped_column(primary_key=True)\n{field}\n"
            ),
            Self::Tortoise => format!(
                "from tortoise import models, fields\n\nclass Job(models.Model):\n    name = fields.TextField()\n{field}\n    class Meta:\n        table = 'jobs'\n"
            ),
        }
    }

    fn mapped(self) -> &'static str {
        match self {
            Self::SqlAlchemy => "    signed_manifest: Mapped[bytes]",
            Self::Tortoise => "    signed_manifest = fields.BinaryField()",
        }
    }

    fn migration(self) -> &'static str {
        match self {
            Self::SqlAlchemy => "alembic/versions/0002_drop.py",
            Self::Tortoise => "migrations/models/2_drop.py",
        }
    }

    fn drop(self, column: &str) -> String {
        match self {
            Self::SqlAlchemy => format!(
                "from alembic import op\nrevision = '002'\ndown_revision = None\n\ndef upgrade():\n    op.drop_column('jobs', '{column}')\n\ndef downgrade():\n    pass\n"
            ),
            Self::Tortoise => format!(
                "async def upgrade(db):\n    return 'ALTER TABLE \"jobs\" DROP COLUMN \"{column}\";'\n\nasync def downgrade(db):\n    return ''\n"
            ),
        }
    }
}

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "fixture"]);
}

fn config(orm: Orm, paths: &str) -> String {
    format!(
        "[tool.zdm]\nallowed-file-patterns = ['app/**', 'pyproject.toml', 'zero-downtime-migrations.toml']\n\n[[tool.zdm.model-sources]]\norm = '{}'\npaths = [{paths}]\n",
        orm.name()
    )
}

fn repo(orm: Orm, model: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.name", "ZDM Test"]);
    git(dir.path(), &["config", "user.email", "zdm@example.com"]);
    write(dir.path(), "pyproject.toml", &config(orm, "'app'"));
    write(dir.path(), "app/__init__.py", "");
    write(dir.path(), "app/models.py", model);
    commit(dir.path());
    git(dir.path(), &["branch", "origin/main"]);
    dir
}

fn add_drop(dir: &TempDir, orm: Orm, column: &str) {
    write(dir.path(), orm.migration(), &orm.drop(column));
}

fn check(dir: &TempDir, args: &[&str], allowed: bool) -> Value {
    let output = cargo_bin_cmd!("zdm")
        .current_dir(dir.path())
        .args(args)
        .args(["--select", "R005,R008", "--output-format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(if allowed { 0 } else { 1 }),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = json["diagnostics"].as_array().unwrap();
    let r005 = diagnostics
        .iter()
        .filter(|d| d["rule"] == "R005" || d["rule_id"] == "R005")
        .count();
    if allowed {
        assert_eq!(r005, 0, "{stdout}");
    } else {
        assert!(r005 > 0, "R005 must be the blocker: {stdout}");
    }
    assert!(
        !diagnostics
            .iter()
            .any(|d| d["rule"] == "R008" || d["rule_id"] == "R008"),
        "Model fixture must not fail solely through R008: {stdout}"
    );
    json
}

#[test]
fn earlier_compatibility_removal_allows_later_drop_for_both_orms() {
    for orm in [Orm::SqlAlchemy, Orm::Tortoise] {
        let dir = repo(orm, &orm.model(orm.mapped()));
        write(dir.path(), "app/models.py", &orm.model(""));
        commit(dir.path());
        git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], true);
    }
}

#[test]
fn audited_cross_module_bases_preserve_orm_and_require_regular_packages() {
    for orm in [Orm::SqlAlchemy, Orm::Tortoise] {
        for package in [true, false] {
            let (base, current) = match orm {
                Orm::SqlAlchemy => (
                    "from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\n",
                    orm.model("").replace("class Base(DeclarativeBase):\n    pass", "from .base import Base"),
                ),
                Orm::Tortoise => (
                    "from tortoise import models\nclass Base(models.Model):\n    class Meta:\n        abstract = True\n",
                    orm.model("").replace("class Job(models.Model):", "from .base import Base\nclass Job(Base):"),
                ),
            };
            let dir = repo(orm, &current);
            write(dir.path(), "app/base.py", base);
            if !package {
                std::fs::remove_file(dir.path().join("app/__init__.py")).unwrap();
            }
            commit(dir.path());
            git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
            add_drop(&dir, orm, "signed_manifest");
            commit(dir.path());
            let result = check(&dir, &["--diff", "origin/main"], package);
            if !package {
                assert!(
                    result.to_string().contains("missing dependency context"),
                    "{result}"
                );
            }
        }
    }
}

#[test]
fn still_mapped_base_and_same_pr_removal_are_rejected() {
    for orm in [Orm::SqlAlchemy, Orm::Tortoise] {
        for remove_now in [false, true] {
            let dir = repo(orm, &orm.model(orm.mapped()));
            if remove_now {
                write(dir.path(), "app/models.py", &orm.model(""));
            }
            add_drop(&dir, orm, "signed_manifest");
            commit(dir.path());
            let result = check(&dir, &["--diff", "origin/main"], false);
            assert!(
                result.to_string().contains("app/models.py"),
                "diagnostic must identify model usage: {result}"
            );
        }
    }
}

#[test]
fn physical_name_aliases_remain_mapped() {
    for (orm, field) in [
        (
            Orm::SqlAlchemy,
            "    manifest: Mapped[bytes] = mapped_column('signed_manifest')",
        ),
        (
            Orm::Tortoise,
            "    manifest = fields.BinaryField(source_field='signed_manifest')",
        ),
    ] {
        let dir = repo(orm, &orm.model(field));
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn tortoise_generated_relation_and_primary_key_columns_are_in_use() {
    for (field, column) in [
        (
            "    owner = fields.ForeignKeyField('models.Owner')",
            "owner_id",
        ),
        ("", "id"),
    ] {
        let orm = Orm::Tortoise;
        let dir = repo(orm, &orm.model(field));
        add_drop(&dir, orm, column);
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn missing_or_dynamic_table_is_unknown_not_absence() {
    for orm in [Orm::SqlAlchemy, Orm::Tortoise] {
        for model in [
            orm.model("").replace("'jobs'", "'other_jobs'"),
            orm.model("").replace("'jobs'", "choose_table()"),
            format!(
                "{}\nclass Dynamic(Job):\n{}\n",
                orm.model(""),
                if matches!(orm, Orm::SqlAlchemy) {
                    "    __tablename__ = choose_table()"
                } else {
                    "    class Meta:\n        table = choose_table()"
                }
            ),
        ] {
            let dir = repo(orm, &model);
            add_drop(&dir, orm, "signed_manifest");
            commit(dir.path());
            check(&dir, &["--diff", "origin/main"], false);
        }
    }
}

#[test]
fn head_reintroduction_is_rejected_despite_permissive_r008() {
    for orm in [Orm::SqlAlchemy, Orm::Tortoise] {
        let dir = repo(orm, &orm.model(""));
        write(dir.path(), "app/models.py", &orm.model(orm.mapped()));
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn head_does_not_read_uncommitted_model_reintroduction() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    write(dir.path(), "app/models.py", &orm.model(orm.mapped()));
    check(&dir, &["--diff", "origin/main"], true);
}

#[test]
fn staged_check_reads_index_not_worktree_in_both_directions() {
    for staged_usage in [false, true] {
        let orm = Orm::SqlAlchemy;
        let dir = repo(orm, &orm.model(""));
        add_drop(&dir, orm, "signed_manifest");
        if staged_usage {
            write(dir.path(), "app/models.py", &orm.model(orm.mapped()));
        }
        git(dir.path(), &["add", "."]);
        write(
            dir.path(),
            "app/models.py",
            &orm.model(if staged_usage { "" } else { orm.mapped() }),
        );
        check(&dir, &["--diff-staged", "origin/main"], !staged_usage);
    }
}

#[test]
fn changed_config_cannot_hide_base_usage_or_grant_new_evidence() {
    let orm = Orm::SqlAlchemy;
    for remove_base_config in [false, true] {
        let dir = repo(
            orm,
            &orm.model(if remove_base_config { "" } else { orm.mapped() }),
        );
        if remove_base_config {
            write(
                dir.path(),
                "pyproject.toml",
                "[tool.zdm]\nallowed-file-patterns = ['pyproject.toml']\n",
            );
            commit(dir.path());
            git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
        }
        write(
            dir.path(),
            "pyproject.toml",
            &config(
                orm,
                if remove_base_config {
                    "'app'"
                } else {
                    "'app/safe'"
                },
            ),
        );
        write(dir.path(), "app/safe/models.py", &orm.model(""));
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn migration_filters_do_not_hide_model_usage() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(orm.mapped()));
    let mut settings = config(orm, "'app'");
    settings = settings.replace("[tool.zdm]\n", "[tool.zdm]\nexclude = ['app/**']\n");
    write(dir.path(), "pyproject.toml", &settings);
    commit(dir.path());
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main", "alembic/versions"], false);
}

#[test]
fn standalone_config_uses_same_historical_contract() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    fs::remove_file(dir.path().join("pyproject.toml")).unwrap();
    write(
        dir.path(),
        "zero-downtime-migrations.toml",
        "[[model-sources]]\norm = 'sqlalchemy'\npaths = ['app']\n",
    );
    commit(dir.path());
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], true);
}

#[test]
fn standalone_scan_cannot_establish_prior_removal() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    add_drop(&dir, orm, "signed_manifest");
    check(&dir, &[orm.migration()], false);
}

#[test]
fn compound_aerich_drop_cannot_borrow_simple_drop_evidence() {
    let orm = Orm::Tortoise;
    let dir = repo(orm, &orm.model(""));
    write(dir.path(), orm.migration(), "async def upgrade(db):\n    return 'ALTER TABLE \"jobs\" DROP COLUMN \"signed_manifest\", DROP COLUMN \"name\";'\n");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn newer_target_branch_does_not_replace_actual_merge_base() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(orm.mapped()));
    git(dir.path(), &["checkout", "-qb", "drop-branch"]);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    git(dir.path(), &["checkout", "-q", "origin/main"]);
    write(dir.path(), "app/models.py", &orm.model(""));
    commit(dir.path());
    git(dir.path(), &["checkout", "-q", "drop-branch"]);
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn removed_model_class_is_unknown_instead_of_empty_mapping() {
    for orm in [Orm::SqlAlchemy, Orm::Tortoise] {
        let dir = repo(orm, &orm.model(""));
        write(
            dir.path(),
            "app/models.py",
            "# Models have moved outside configured coverage.\n",
        );
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn inherited_columns_are_present_even_when_child_omits_them() {
    for (orm, model) in [
        (Orm::SqlAlchemy, "from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\nclass Base(DeclarativeBase):\n    pass\nclass ManifestMixin:\n    signed_manifest: Mapped[bytes]\nclass Job(ManifestMixin, Base):\n    __tablename__ = 'jobs'\n    id: Mapped[int] = mapped_column(primary_key=True)\n"),
        (Orm::Tortoise, "from tortoise import models, fields\nclass JobBase(models.Model):\n    signed_manifest = fields.BinaryField()\n    class Meta:\n        abstract = True\nclass Job(JobBase):\n    name = fields.TextField()\n    class Meta:\n        table = 'jobs'\n"),
    ] {
        let dir = repo(orm, model);
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn additional_mapping_of_same_table_cannot_be_ignored() {
    let orm = Orm::SqlAlchemy;
    let model = format!("{}\nclass OldJob(Base):\n    __tablename__ = 'jobs'\n    __table_args__ = {{'extend_existing': True}}\n    id: Mapped[int] = mapped_column(primary_key=True)\n    signed_manifest: Mapped[bytes]\n", orm.model(""));
    let dir = repo(orm, &model);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn model_paths_normalize_current_directory_components() {
    let orm = Orm::SqlAlchemy;
    for paths in ["'.'", "'./app'", "'.', './app'"] {
        let dir = repo(orm, &orm.model(""));
        write(dir.path(), "pyproject.toml", &config(orm, paths));
        write(dir.path(), "alembic/__init__.py", "");
        write(dir.path(), "alembic/versions/__init__.py", "");
        commit(dir.path());
        git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], true);
    }
}

#[test]
fn model_paths_cannot_escape_repository() {
    let orm = Orm::SqlAlchemy;
    for invalid in ["'../app'", "'/tmp/models'", "'app/../../outside'"] {
        let dir = repo(orm, &orm.model(""));
        write(dir.path(), "pyproject.toml", &config(orm, invalid));
        commit(dir.path());
        git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
        add_drop(&dir, orm, "signed_manifest");
        commit(dir.path());
        let output = cargo_bin_cmd!("zdm")
            .current_dir(dir.path())
            .args(["--diff", "origin/main", "--select", "R005"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "unsafe model source accepted: {invalid}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_source_is_unknown_instead_of_followed() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    write(dir.path(), "external/models.py", &orm.model(orm.mapped()));
    std::os::unix::fs::symlink("../external/models.py", dir.path().join("app/linked.py")).unwrap();
    commit(dir.path());
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn gitlink_inside_model_coverage_is_unknown() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let oid = String::from_utf8(output.stdout).unwrap();
    git(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},app/vendor", oid.trim()),
        ],
    );
    git(dir.path(), &["commit", "-qm", "model gitlink"]);
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    add_drop(&dir, orm, "signed_manifest");
    // The absent worktree gitlink must not be staged as a deletion.
    git(dir.path(), &["add", orm.migration()]);
    git(dir.path(), &["commit", "-qm", "drop"]);
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn non_utf8_source_is_unknown_instead_of_skipped() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    fs::write(
        dir.path().join("app/legacy.py"),
        b"# coding: latin-1\nvalue = '\xff'\n",
    )
    .unwrap();
    commit(dir.path());
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn another_orm_mapping_cannot_hide_present_column() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    write(
        dir.path(),
        "app/legacy.py",
        &Orm::Tortoise.model(Orm::Tortoise.mapped()),
    );
    let settings = format!(
        "{}\n[[tool.zdm.model-sources]]\norm = 'tortoise'\npaths = ['app']\n",
        config(orm, "'app'")
    );
    write(dir.path(), "pyproject.toml", &settings);
    commit(dir.path());
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn model_schema_mismatch_does_not_authorize_drop() {
    let orm = Orm::SqlAlchemy;
    let model = orm.model("").replace(
        "    __tablename__ = 'jobs'",
        "    __tablename__ = 'jobs'\n    __table_args__ = {'schema': 'archive'}",
    );
    let dir = repo(orm, &model);
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn quoted_aerich_name_matches_actual_database_alias() {
    let orm = Orm::Tortoise;
    let dir = repo(
        orm,
        &orm.model("    manifest = fields.BinaryField(source_field='SignedManifest')"),
    );
    add_drop(&dir, orm, "SignedManifest");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn aerich_unsupported_targets_and_escaped_identifiers_stay_blocked() {
    let orm = Orm::Tortoise;
    for sql in [
        "ALTER TABLE ONLY jobs DROP COLUMN signed_manifest;",
        "ALTER TABLE jobs DROP COLUMN IF EXISTS signed_manifest;",
        "ALTER TABLE \"jo\"\"bs\" DROP COLUMN \"signed_manifest\";",
        "ALTER TABLE jobs DROP COLUMN \"signed\"\"manifest\";",
    ] {
        let dir = repo(orm, &orm.model(""));
        write(
            dir.path(),
            orm.migration(),
            &format!("async def upgrade(db):\n    return '{sql}'\n"),
        );
        commit(dir.path());
        check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn model_evidence_does_not_disable_changeset_separation() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    add_drop(&dir, orm, "signed_manifest");
    write(dir.path(), "service.py", "print('changed application')\n");
    commit(dir.path());
    let output = cargo_bin_cmd!("zdm")
        .current_dir(dir.path())
        .args([
            "--diff",
            "origin/main",
            "--select",
            "R005,R008",
            "--output-format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = result["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics.iter().any(|d| d["rule_id"] == "R008"),
        "{result}"
    );
    assert!(
        !diagnostics.iter().any(|d| d["rule_id"] == "R005"),
        "{result}"
    );
}

#[test]
fn disabling_r005_does_not_require_model_evidence() {
    let orm = Orm::SqlAlchemy;
    let dir = repo(orm, &orm.model(""));
    fs::write(dir.path().join("app/legacy.py"), [0xff]).unwrap();
    add_drop(&dir, orm, "signed_manifest");
    commit(dir.path());
    cargo_bin_cmd!("zdm")
        .current_dir(dir.path())
        .args(["--diff", "origin/main", "--select", "R008"])
        .assert()
        .success();
}

#[test]
fn sql_comment_delimiters_in_identifiers_do_not_change_the_model_target() {
    let dir = repo(Orm::Tortoise, "from tortoise import models, fields\nclass Clean(models.Model):\n    class Meta:\n        table = 'jobs '\nclass Live(models.Model):\n    signed_manifest = fields.BinaryField()\n    class Meta:\n        table = 'jobs/*x*/'\n");
    write(dir.path(), Orm::Tortoise.migration(), "async def upgrade(db):\n    return 'ALTER TABLE \"jobs/*x*/\" DROP COLUMN signed_manifest;'\n");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn quoted_semicolons_cannot_authorize_a_fictitious_drop() {
    let dir = repo(Orm::Tortoise, "from tortoise import models, fields\nclass Clean(models.Model):\n    class Meta:\n        table = 'clean'\nclass Live(models.Model):\n    signed_manifest = fields.BinaryField()\n    class Meta:\n        table = 'jobs; ALTER TABLE clean DROP COLUMN retired;'\n");
    write(dir.path(), Orm::Tortoise.migration(), "async def upgrade(db):\n    return 'ALTER TABLE \"jobs; ALTER TABLE clean DROP COLUMN retired;\" DROP COLUMN signed_manifest;'\n");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], false);
}

const GATEWAY_MODEL: &str = "app/ai_tools/persistence/generation_records.py";
const GATEWAY_BASE: &str = "app/persistence/models.py";
const GATEWAY_OPERATIONS: &str = "app/ai_generators/persistence/module_operations.py";
const GATEWAY_PROVENANCE: &str = include_str!("fixtures/models/gateway/generation_records.py");

fn gateway_config(paths: &str) -> String {
    config(Orm::SqlAlchemy, paths).replace(
        "[tool.zdm]\n",
        "[tool.zdm]\nalembic-version-locations = ['migrations/versions', 'migrations/beta_versions']\n",
    )
}

fn gateway_mapped() -> String {
    format!("{GATEWAY_PROVENANCE}\n    signed_manifest: Mapped[bytes]\n")
}

fn gateway_repo(earlier_removal: bool) -> TempDir {
    let dir = repo(Orm::SqlAlchemy, "");
    write(
        dir.path(),
        "pyproject.toml",
        &gateway_config(&format!(
            "'{GATEWAY_BASE}', '{GATEWAY_MODEL}', '{GATEWAY_OPERATIONS}'"
        )),
    );
    write(
        dir.path(),
        GATEWAY_BASE,
        include_str!("fixtures/models/gateway/base.py"),
    );
    write(
        dir.path(),
        GATEWAY_OPERATIONS,
        include_str!("fixtures/models/gateway/module_operations.py"),
    );
    for package in [
        "app",
        "app/persistence",
        "app/ai_tools",
        "app/ai_tools/persistence",
        "app/ai_generators",
        "app/ai_generators/persistence",
    ] {
        write(dir.path(), &format!("{package}/__init__.py"), "");
    }
    write(dir.path(), GATEWAY_MODEL, &gateway_mapped());
    commit(dir.path());
    if earlier_removal {
        write(dir.path(), GATEWAY_MODEL, GATEWAY_PROVENANCE);
        commit(dir.path());
    }
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    dir
}

fn gateway_drop(dir: &TempDir, location: &str) {
    write(dir.path(), &format!("{location}/20261020_0044_drop.py"), "from alembic import op\nrevision = '0044'\ndown_revision = '0043'\ndef upgrade():\n    op.drop_column('ai_tool_generation_provenance', 'signed_manifest')\ndef downgrade():\n    pass\n");
}

fn gateway_check(dir: &TempDir, staged: bool, allowed: bool) -> Value {
    if staged {
        git(dir.path(), &["add", "."]);
    } else {
        commit(dir.path());
    }
    check(
        dir,
        &[
            if staged { "--diff-staged" } else { "--diff" },
            "origin/main",
        ],
        allowed,
    )
}

#[test]
fn gateway_prior_removal_allows_main_and_beta_drop_in_head_and_index() {
    for location in ["migrations/versions", "migrations/beta_versions"] {
        for staged in [false, true] {
            let dir = gateway_repo(true);
            gateway_drop(&dir, location);
            gateway_check(&dir, staged, true);
            // Neither dirty mapping reintroduction nor a broken worktree base changes evidence.
            write(dir.path(), GATEWAY_MODEL, &gateway_mapped());
            write(dir.path(), GATEWAY_BASE, "invalid python !!!");
            check(
                &dir,
                &[
                    if staged { "--diff-staged" } else { "--diff" },
                    "origin/main",
                ],
                true,
            );
        }
    }
}

#[test]
fn gateway_same_pr_and_retained_mappings_are_rejected_by_r005() {
    for remove_now in [false, true] {
        for staged in [false, true] {
            let dir = gateway_repo(false);
            if remove_now {
                write(dir.path(), GATEWAY_MODEL, GATEWAY_PROVENANCE);
            }
            gateway_drop(&dir, "migrations/beta_versions");
            let result = gateway_check(&dir, staged, false);
            assert!(result
                .to_string()
                .contains("merge-base: column signed_manifest is still mapped"));
        }
    }
}

#[test]
fn gateway_reintroduction_duplicate_and_missing_dependency_cannot_authorize_drop() {
    for scenario in ["reintroduce", "duplicate", "dependency", "new-coverage"] {
        for staged in [false, true] {
            let dir = gateway_repo(true);
            match scenario {
                "reintroduce" => write(dir.path(), GATEWAY_MODEL, &gateway_mapped()),
                "duplicate" => write(dir.path(), GATEWAY_MODEL, &format!("{GATEWAY_PROVENANCE}\nclass OtherProvenance(Base):\n    __tablename__ = 'ai_tool_generation_provenance'\n    manifest: Mapped[bytes] = mapped_column('signed_manifest')\n")),
                "dependency" => { fs::remove_file(dir.path().join(GATEWAY_BASE)).unwrap(); }
                "new-coverage" => {
                    write(dir.path(), "pyproject.toml", &gateway_config(&format!("'{GATEWAY_BASE}', '{GATEWAY_OPERATIONS}'")));
                    commit(dir.path());
                    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
                    write(dir.path(), "pyproject.toml", &gateway_config(&format!("'{GATEWAY_BASE}', '{GATEWAY_MODEL}', '{GATEWAY_OPERATIONS}'")));
                }
                _ => unreachable!(),
            }
            gateway_drop(&dir, "migrations/beta_versions");
            let result = gateway_check(&dir, staged, false);
            if scenario == "reintroduce" || scenario == "duplicate" {
                assert!(
                    result
                        .to_string()
                        .contains("checked revision: column signed_manifest is still mapped"),
                    "{scenario}: {result}"
                );
            }
        }
    }
}

fn table_check(dir: &TempDir, args: &[&str], allowed: bool) -> Value {
    let output = cargo_bin_cmd!("zdm")
        .current_dir(dir.path())
        .args(args)
        .args(["--select", "R019", "--output-format", "json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(if allowed { 0 } else { 1 }),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["diagnostics"].as_array().unwrap().is_empty(), allowed);
    json
}

fn table_retirement(orm: Orm) -> TempDir {
    let dir = repo(orm, &orm.model(""));
    let prior = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    write(
        dir.path(),
        "zero-downtime-migrations.toml",
        &format!(
            "model-retirement-ref = '{}'\n",
            String::from_utf8(prior.stdout).unwrap().trim()
        ),
    );
    write(dir.path(), "app/models.py", "# retired mappings\n");
    commit(dir.path());
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    let drop = match orm {
        Orm::SqlAlchemy => "from alembic import op\ndef upgrade():\n    op.drop_table('jobs')\n",
        Orm::Tortoise => "async def upgrade(db):\n    return 'DROP TABLE jobs;'\n",
    };
    write(dir.path(), orm.migration(), drop);
    dir
}

#[test]
fn table_retirement_requires_prior_mapping_and_earlier_removal() {
    for orm in [Orm::SqlAlchemy, Orm::Tortoise] {
        let dir = table_retirement(orm);
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], true);
        write(
            dir.path(),
            "app/models.py",
            &orm.model("").replace("jobs", "other"),
        );
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], true);
        table_check(&dir, &[orm.migration()], false);
        write(dir.path(), "app/models.py", &orm.model(""));
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], false);
        write(dir.path(), "app/models.py", "# retired\n");
        commit(dir.path());
        git(dir.path(), &["branch", "-f", "origin/main", "HEAD~1"]);
        // Keep migration in diff after making the mapped revision the base.
        write(
            dir.path(),
            orm.migration(),
            &format!(
                "{}\n# changed\n",
                fs::read_to_string(dir.path().join(orm.migration())).unwrap()
            ),
        );
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn table_retirement_uses_index_and_base_established_configuration() {
    let dir = table_retirement(Orm::SqlAlchemy);
    git(dir.path(), &["add", "."]);
    write(dir.path(), "app/models.py", &Orm::SqlAlchemy.model(""));
    table_check(&dir, &["--diff-staged", "origin/main"], true);
    git(dir.path(), &["add", "."]);
    write(dir.path(), "app/models.py", "# retired\n");
    table_check(&dir, &["--diff-staged", "origin/main"], false);
}

#[test]
fn table_retirement_rejects_unknown_coverage_and_retained_references() {
    for model in [
        "from sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy import Integer\nfrom missing import foreign_keys\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = mapped_column(Integer, foreign_keys[0])\n",
        "from sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy.sql.schema import ForeignKey\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = mapped_column(ForeignKey('jobs.id'))\n",
        "from sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy import Integer, ForeignKey\ndef make_fk():\n    return ForeignKey('jobs.id')\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = mapped_column(Integer, make_fk())\n",
        "from sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy import Integer\nfrom missing import foreign_key\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = mapped_column(Integer, foreign_key)\n",
        "from missing import Base\nclass Unknown(Base):\n    __tablename__ = 'other'\n",
        "from sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy import ForeignKey\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = mapped_column(ForeignKey('jobs.id'))\n",
        "from sqlalchemy.orm import DeclarativeBase, relationship\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = relationship('Job')\n",
    ] {
        let dir = table_retirement(Orm::SqlAlchemy);
        write(dir.path(), "app/models.py", model);
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn table_retirement_never_authorizes_a_rename() {
    let dir = table_retirement(Orm::SqlAlchemy);
    write(
        dir.path(),
        Orm::SqlAlchemy.migration(),
        "from alembic import op\ndef upgrade():\n    op.rename_table('jobs', 'other')\n",
    );
    commit(dir.path());
    table_check(&dir, &["--diff", "origin/main"], false);
}

#[test]
fn table_retirement_cannot_fabricate_or_replace_the_historical_witness() {
    for witness in [
        "HEAD",
        "0000000000000000000000000000000000000000",
        "base",
        "missing",
        "unknown",
        "absent",
    ] {
        let dir = table_retirement(Orm::SqlAlchemy);
        if matches!(witness, "unknown" | "absent") {
            // Make a different ancestral snapshot with no usable historical mapping.
            write(
                dir.path(),
                "app/models.py",
                if witness == "unknown" {
                    "from missing import Base\nclass Job(Base):\n    __tablename__ = 'jobs'\n"
                } else {
                    "# no mapping\n"
                },
            );
            commit(dir.path());
        }
        let value = if matches!(witness, "base" | "unknown" | "absent") {
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir.path())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        } else {
            witness.into()
        };
        write(
            dir.path(),
            "zero-downtime-migrations.toml",
            &if witness == "missing" {
                String::new()
            } else {
                format!("model-retirement-ref = '{value}'\n")
            },
        );
        write(dir.path(), "app/models.py", "# retired\n");
        commit(dir.path());
        git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
        write(
            dir.path(),
            Orm::SqlAlchemy.migration(),
            "from alembic import op\ndef upgrade():\n    op.drop_table('jobs')\n# contraction\n",
        );
        // A good SHA added only in this PR cannot replace a bad base setting.
        let output = Command::new("git")
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        write(
            dir.path(),
            "zero-downtime-migrations.toml",
            &format!(
                "model-retirement-ref = '{}'\n",
                String::from_utf8(output.stdout).unwrap().trim()
            ),
        );
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn table_retirement_proof_rejects_schema_ambiguity_and_unresolved_relations() {
    for (orm, model, allowed) in [
        (Orm::SqlAlchemy, "from sqlalchemy.orm import DeclarativeBase\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'jobs'\n    __table_args__ = {'schema': 'other'}\n", false),
        (Orm::SqlAlchemy, "from sqlalchemy.orm import DeclarativeBase\nfrom sqlalchemy import ForeignKeyConstraint\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    __table_args__ = (ForeignKeyConstraint(['job'], ['jobs.id']),)\n", false),
        (Orm::SqlAlchemy, "from sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy import ForeignKey\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = mapped_column(ForeignKey(target()))\n", false),
        (Orm::SqlAlchemy, "from sqlalchemy.orm import DeclarativeBase, mapped_column\nfrom sqlalchemy import ForeignKey\nclass Base(DeclarativeBase):\n    pass\nclass Other(Base):\n    __tablename__ = 'other'\n    job = mapped_column(ForeignKey('retained.id'))\n", true),
        (Orm::Tortoise, "from tortoise import models, fields\nclass Other(models.Model):\n    job = fields.ForeignKeyField('models.Job')\n    class Meta:\n        table = 'other'\n", false),
    ] {
        let dir = table_retirement(orm);
        write(dir.path(), "app/models.py", model);
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], allowed);
    }
}

const RECORD_CONTEXT: &str = r#"from typing import Any, ClassVar, NamedTuple, Self
from dataclasses import dataclass
import re
from .faults import ServiceError
PATTERN = re.compile(pattern())
logger = logging.get_logger(__name__)
message = translate("title")
adapter = TypeAdapter(Sequence[Unknown])
@decorate()
def unrelated(value=initialize()) -> Unknown:
    pass
class Field(NamedTuple):
    text: str
    description: str
class RecordError(ServiceError):
    def __init__(self, message: str) -> None:
        super().__init__(message)
@dataclass(frozen=True, slots=True)
class Key:
    name: str
    value: Any
    namespace: str = "example"
    def __str__(self) -> str:
        return self.name
    @classmethod
    def parse(cls, raw: str = initialize()) -> Self | None:
        return cls(raw, None)
@dataclass(frozen=True, slots=True)
class Entry:
    key: Key
    text: str
"#;

fn metadata_retirement(context: &str, metadata: &str, dependency: &str) -> TempDir {
    metadata_retirement_scoped(
        context,
        metadata,
        dependency,
        "'app/models.py', 'app/records.py'",
    )
}

fn metadata_retirement_scoped(
    context: &str,
    metadata: &str,
    dependency: &str,
    paths: &str,
) -> TempDir {
    let orm = Orm::SqlAlchemy;
    let current = format!("from typing import Annotated, ClassVar\nfrom sqlalchemy import ForeignKey\nfrom .records import Field\n{}\nJobRef = Annotated[int, mapped_column(ForeignKey('jobs.id'))]\nclass Other(Base):\n    __tablename__ = 'other'\n    id: Mapped[int] = mapped_column(primary_key=True)\n    {}\n", orm.model(""), metadata);
    let dir = repo(orm, &current);
    write(dir.path(), "app/__init__.py", "");
    write(dir.path(), "app/records.py", context);
    write(
        dir.path(),
        "app/faults/__init__.py",
        "from .base import ServiceError\n",
    );
    write(dir.path(), "app/faults/base.py", dependency);
    write(dir.path(), "pyproject.toml", &config(orm, paths));
    commit(dir.path());
    let prior = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    write(
        dir.path(),
        "zero-downtime-migrations.toml",
        &format!(
            "model-retirement-ref = '{}'\n",
            String::from_utf8(prior.stdout).unwrap().trim()
        ),
    );
    write(
        dir.path(),
        "app/models.py",
        &current.replace("__tablename__ = 'jobs'", "__tablename__ = 'retained'"),
    );
    commit(dir.path());
    git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
    write(
        dir.path(),
        orm.migration(),
        "from alembic import op\ndef upgrade():\n    op.drop_table('jobs')\n",
    );
    dir
}

#[test]
fn tortoise_meta_objects_cannot_authorize_table_retirement() {
    for (staged, metadata) in [
        (false, "Meta: ClassVar[MetaRecord] = MetaRecord('jobs')"),
        (true, "Meta: ClassVar[MetaRecord] = MetaRecord('jobs')"),
        (false, "Meta: ClassVar = MetaConfig"),
        (
            false,
            "class Meta:\n        abstract = True\n    Meta: ClassVar = MetaConfig",
        ),
    ] {
        let dir = table_retirement(Orm::Tortoise);
        fs::remove_file(dir.path().join(Orm::Tortoise.migration())).unwrap();
        let source = format!("from typing import ClassVar, NamedTuple\nfrom tortoise import models, fields\nclass MetaRecord(NamedTuple):\n    table: str\nclass MetaConfig:\n    table = 'jobs'\nclass Job(models.Model):\n    id = fields.IntField(primary_key=True)\n    {metadata}\n");
        write(dir.path(), "app/models.py", &source);
        commit(dir.path());
        git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
        write(
            dir.path(),
            Orm::Tortoise.migration(),
            "async def upgrade(db):\n    return 'DROP TABLE jobs;'\n",
        );
        if staged {
            git(dir.path(), &["add", "."]);
        } else {
            commit(dir.path());
        }
        let mode = if staged { "--diff-staged" } else { "--diff" };
        let result = table_check(&dir, &[mode, "origin/main"], false);
        assert!(result.to_string().contains("(Meta)"), "{result}");
        assert!(
            !result.to_string().contains("work limit exceeded"),
            "{result}"
        );
    }
}

#[test]
fn audited_static_metadata_uses_complete_immutable_constructor_context() {
    let metadata = "public_id: Mapped[str] = mapped_column(default=lambda: make_id())\n    def helper(self, value=initialize()):\n        pass\n    __labels__: ClassVar[dict[str, Field]] = {'title': Field('translated', description='Display text')}\n    __lookup__: ClassVar[str] = 'key'\n    __strict__: ClassVar[bool] = True\n    __bare__: ClassVar = 'literal'";
    let dependency = "class ServiceError(Exception):\n    def __init__(self, message: str) -> None:\n        self.message = message\n";
    for (staged, paths) in [
        (false, "'app/models.py', 'app/records.py'"),
        (true, "'app/models.py'"),
    ] {
        let context = RECORD_CONTEXT.replace(
            "class Field(NamedTuple):",
            "class OptionalValue(NamedTuple):\n    text: str\nclass Field(NamedTuple):",
        );
        let context = context.replace(
            "    description: str",
            "    description: str\n    value: OptionalValue | None = None",
        );
        let context = if staged {
            format!("{context}\nclass Unrelated:\n    class Nested: pass\ndef duplicate(): pass\ndef duplicate(): pass\n")
        } else {
            context
        };
        let dir = metadata_retirement_scoped(&context, metadata, dependency, paths);
        if staged {
            git(dir.path(), &["add", "."]);
        } else {
            commit(dir.path());
        }
        table_check(
            &dir,
            &[
                if staged { "--diff-staged" } else { "--diff" },
                "origin/main",
            ],
            true,
        );
        // The selected dependency comes from the snapshot, never the dirty worktree.
        write(
            dir.path(),
            "app/records.py",
            "class Field(dynamic_base()):\n    pass\n",
        );
        table_check(
            &dir,
            &[
                if staged { "--diff-staged" } else { "--diff" },
                "origin/main",
            ],
            true,
        );
        git(dir.path(), &["add", "."]);
        table_check(&dir, &["--diff-staged", "origin/main"], false);
    }
    for (name, context, value, base) in [
        (
            "audited-class-decorator",
            format!("{RECORD_CONTEXT}\n@unknown_decorator\nclass Sibling: pass\n"),
            metadata.into(),
            dependency.to_owned(),
        ),
        (
            "ancestor-class-decorator",
            RECORD_CONTEXT.to_owned(),
            metadata.into(),
            format!("@unknown_decorator\n{dependency}"),
        ),
        (
            "mapped-value",
            RECORD_CONTEXT.to_owned(),
            metadata.replace(
                "Field('translated', description='Display text')",
                "mapped_column()",
            ),
            dependency.to_owned(),
        ),
        (
            "duplicate-dict-value",
            RECORD_CONTEXT.to_owned(),
            metadata.replace(
                "{'title': Field",
                "{'title': mapped_column(ForeignKey('jobs.id')), 'title': Field",
            ),
            dependency.to_owned(),
        ),
        (
            "annotations-protocol",
            RECORD_CONTEXT.to_owned(),
            "__annotations__: ClassVar[dict[str, str]] = {'job_id': 'Mapped[JobRef]'}".into(),
            dependency.to_owned(),
        ),
        (
            "allow-unmapped-protocol",
            RECORD_CONTEXT.to_owned(),
            "__allow_unmapped__: ClassVar[str] = 'metadata'".into(),
            dependency.to_owned(),
        ),
        (
            "type-annotation-map-protocol",
            RECORD_CONTEXT.to_owned(),
            "type_annotation_map: ClassVar[dict[str, str]] = {}".into(),
            dependency.to_owned(),
        ),
        (
            "class-namespace-hook",
            RECORD_CONTEXT.to_owned(),
            "__module__: ClassVar[str] = 'replacement'".into(),
            dependency.to_owned(),
        ),
        (
            "dataclass-mapping-hook",
            RECORD_CONTEXT.to_owned(),
            "__sa_dataclass_metadata_key__: ClassVar[str] = 'metadata'".into(),
            dependency.to_owned(),
        ),
        (
            "mapping-hook",
            RECORD_CONTEXT.to_owned(),
            "__table_cls__: ClassVar[str] = 'key'".into(),
            dependency.to_owned(),
        ),
        (
            "mapper-lifecycle-hook",
            RECORD_CONTEXT.to_owned(),
            "def __declare_last__():\n        register_mapping()".into(),
            dependency.to_owned(),
        ),
        (
            "constructor-hook",
            RECORD_CONTEXT.replace(
                "class Field(NamedTuple):",
                "class Field(NamedTuple):\n    def __new__(cls):\n        pass",
            ),
            metadata.into(),
            dependency.to_owned(),
        ),
        (
            "dataclass-factory",
            RECORD_CONTEXT.replace(
                "namespace: str = \"example\"",
                "namespace: str = mapped_column()",
            ),
            metadata.into(),
            dependency.to_owned(),
        ),
        (
            "unresolved-base",
            RECORD_CONTEXT.to_owned(),
            metadata.into(),
            dependency.replace("Exception", "Missing"),
        ),
        (
            "rebound-base",
            RECORD_CONTEXT.to_owned(),
            metadata.into(),
            format!("{dependency}\nServiceError = factory()\n"),
        ),
        (
            "mutated-module",
            format!("{RECORD_CONTEXT}\nField.text = mapped_column()\n"),
            metadata.into(),
            dependency.to_owned(),
        ),
    ] {
        let dir = metadata_retirement(&context, &value, &base);
        commit(dir.path());
        let result = table_check(&dir, &["--diff", "origin/main"], false);
        let reason = match name {
            "audited-class-decorator" | "ancestor-class-decorator" => "unresolved class decorator",
            "mapped-value" => "unsupported mapping hook",
            "duplicate-dict-value" => "eager attribute expression",
            "annotations-protocol" => "unsupported mapping hook",
            "class-namespace-hook" => "unsupported mapping hook",
            "dataclass-mapping-hook" => "unsupported mapping hook",
            "mapping-hook" => "unsupported mapping hook",
            "mapper-lifecycle-hook" => "unsupported class-body construct or decorator",
            "constructor-hook" => "mapping annotation is not proven inert",
            "dataclass-factory" => "eager class expression",
            "unresolved-base" => "unresolved base",
            "rebound-base" => "unresolved base",
            "mutated-module" => "mapping annotation is not proven inert",
            "allow-unmapped-protocol" => "unsupported mapping hook",
            "type-annotation-map-protocol" => "unsupported mapping metadata",
            _ => unreachable!(),
        };
        assert!(result.to_string().contains(reason), "{name}: {result}");
        assert!(
            !result.to_string().contains("work limit exceeded"),
            "{name}: {result}"
        );
    }
    for declaration in [
        "public_id: Mapped[str] = mapped_column(default=lambda value: value)",
        "public_id: Mapped[str] = mapped_column(default=lambda value=make_id(): value)",
        "public_id: Mapped[str] = mapped_column(default=(lambda: make_id())())",
        "public_id: Mapped[str] = mapped_column(default=make_id())",
        "public_id: Mapped[str] = mapped_column(default=lambda: mapped_column())",
        "public_id: Mapped[str] = mapped_column(default=lambda: ForeignKey('jobs.id'))",
        "public_id: Mapped[str] = mapped_column(onupdate=lambda: make_id())",
        "__labels__: ClassVar = lambda: make_id()",
        "registry: ClassVar = lambda: make_id()",
        "__table_args__: ClassVar = lambda: make_id()",
        "public_id: Mapped[str] = mapped_column(default=app.records.DEFAULT)",
    ] {
        let dir = metadata_retirement(
            &format!("{RECORD_CONTEXT}\nDEFAULT = lambda: make_id()\n"),
            declaration,
            dependency,
        );
        commit(dir.path());
        let result = table_check(&dir, &["--diff", "origin/main"], false);
        let reason = if declaration.contains("lambda: mapped_column")
            || declaration.contains("lambda: ForeignKey")
        {
            "unsupported explicit ORM declaration"
        } else {
            "eager attribute expression is unresolved"
        };
        assert!(
            result.to_string().contains(reason),
            "{declaration}: {result}"
        );
        assert!(
            !result.to_string().contains("work limit exceeded"),
            "{declaration}: {result}"
        );
    }
}

#[test]
fn audited_root_requires_its_parent_binding_even_without_local_imports() {
    for (parent, allowed) in [
        ("initialize()\ndef __getattr__(name):\n    globals()['catalog'] = construct()\n    return globals()['catalog']\n", true),
        ("models, other = replacement, 0\n", false),
        ("from provider import (__path__, other)\n", false),
        ("__path__, other = replacement, 0\n", false),
    ] {
        let dir = repo(Orm::SqlAlchemy, &Orm::SqlAlchemy.model(""));
        write(dir.path(), "pyproject.toml", &config(Orm::SqlAlchemy, "'app/models.py'"));
        write(dir.path(), "app/__init__.py", parent);
        commit(dir.path());
        git(dir.path(), &["branch", "-f", "origin/main", "HEAD"]);
        add_drop(&dir, Orm::SqlAlchemy, "retired");
        commit(dir.path());
        let result = check(&dir, &["--diff", "origin/main"], allowed);
        if !allowed { assert!(result.to_string().contains("package"), "{result}"); }
    }
}

#[test]
fn audited_constructor_imports_still_require_exact_package_context() {
    let context = "from typing import NamedTuple\nclass Field(NamedTuple):\n    text: str\n    description: str\n";
    for (path, content) in [
        ("app/__init__.py", "__package__ = 'plugins'\nfrom .records import Field\n"),
        ("app/__init__.py", "(__package__ := 'plugins')\nfrom .records import Field\n"),
        ("app/__init__.py", "original_import = __import__\ndef hook(name, globals, locals, fromlist, level):\n    register_mapping()\n    return original_import(name, globals, locals, fromlist, level)\n__builtins__ = {'__import__': hook}\nfrom .records import Field\n"),
        ("app/__init__.py", "from re import compile as __getattr__\n"),
        ("app/__init__.py", "import typing as __builtins__\n"),
        ("app/records.py", "from typing import NamedTuple\nimport typing as __package__\nclass Field(NamedTuple):\n    text: str\n    description: str\n"),
        ("re.py", "# repository-local module shadows stdlib\n"),
        ("re.cpython-312-x86_64-linux-gnu.so", "opaque extension"),
        ("tortoise.pyc", "opaque bytecode"),
        ("app/records.cpython-312-x86_64-linux-gnu.so", "opaque extension"),
        ("app/records/__init__.pyd", "opaque package extension"),
        ("app/__init__.pyd", "opaque parent package extension"),
        ("app/models.abi3.so", "opaque audited model extension"),
        ("app/records.pyx", "compiled source"),
        ("functools.py", "# repository-local module shadows trusted decorator\n"),
        ("dataclasses/__init__.py", "# repository-local package shadows stdlib\n"),
        ("app/records.py", "from typing import NamedTuple\nclass Field(NamedTuple):\n    text: str\n    description: str\ndef replacement(cls, text, description):\n    return tuple.__new__(cls, (text, description))\nf\"{setattr(Field, '__new__', replacement)}\"\n"),
        ("app/records/__init__.py", "# module/package collision\n"),
    ] {
        let dir = metadata_retirement(
            context,
            "__labels__: ClassVar[dict[str, Field]] = {'title': Field('Title', 'Text')}",
            "",
        );
        write(dir.path(), path, content);
        commit(dir.path());
        table_check(&dir, &["--diff", "origin/main"], false);
    }
}

#[test]
fn dependency_resolution_rejects_missing_ambiguous_or_dynamic_bindings() {
    let metadata =
        "__labels__: ClassVar[dict[str, Field]] = {'title': Field('translation', 'Title')}";
    let dependency = "class ServiceError(Exception):\n    pass\n";
    for (path, content) in [
        ("app/faults/base.py", "# missing definition\n"),
        ("app/faults/base/__init__.py", dependency),
        ("app/faults.py", "from .faults.base import ServiceError\n"),
        ("app/faults/__init__.py", "from .base import *\n"),
        (
            "app/faults/__init__.py",
            "if condition:\n    from .base import ServiceError\n",
        ),
        (
            "app/faults/base.py",
            "Exception = factory()\nclass ServiceError(Exception):\n    pass\n",
        ),
        ("app/records.py", "from typing import NamedTuple\nclass Field(NamedTuple):\n    text: str\n    description: str\ndef replacement(cls, text, description):\n    return tuple.__new__(cls, (text, description))\nmarker: setattr(Field, '__new__', replacement) = 0\n"),
        ("app/records.py", "from typing import NamedTuple\nclass Field(NamedTuple):\n    text: str\n    description: str\ndef replacement(cls, text, description):\n    return tuple.__new__(cls, (text, description))\nmarker: setattr(Field, '__new__', replacement)\n"),
        ("app/records.py", "from typing import NamedTuple\ndef install_mapping():\n    return str\nclass Field(NamedTuple):\n    text: str\n    description: str\n    'doc', install_mapping()\n"),
        ("app/records.py", "from typing import NamedTuple\nclass Field(NamedTuple):\n    text: str\n    description: str\ndef replacement(cls, *args):\n    return tuple.__new__(cls, args)\n'doc', setattr(Field, '__new__', replacement)\n"),
        ("app/records.py", "from typing import NamedTuple\nclass Field(NamedTuple):\n    text: str\n    description: str\nclass Mutator:\n    Field.__new__ = factory()\n"),
        ("app/records.py", "from typing import NamedTuple\nclass Field(NamedTuple):\n    text: str\n    description: str\ndef install(value=setattr(Field, '__new__', factory())):\n    pass\n"),
        ("app/records.py", "from typing import NamedTuple\nif True:\n    NamedTuple = factory()\nclass Field(NamedTuple):\n    text: str\n    description: str\n"),
        ("app/records.py", "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass Field(DeclarativeBase):\n    __tablename__ = 'jobs'\n    id: Mapped[int]\n"),
        (
            "app/__init__.py",
            "import sys\nsys.modules['app.faults'] = replacement\n",
        ),

    ] {
        let paths = if path.starts_with("app/faults") { "'app/models.py', 'app/records.py'" } else { "'app/models.py'" };
        let context = if paths == "'app/models.py'" {
            format!("{RECORD_CONTEXT}\nclass Unrelated:\n    class Nested:\n        pass\ndef duplicate(): pass\ndef duplicate(): pass\n")
        } else { RECORD_CONTEXT.to_owned() };
        let dir = metadata_retirement_scoped(&context, metadata, dependency, paths);
        write(dir.path(), path, content);
        commit(dir.path());
        let result = table_check(&dir, &["--diff", "origin/main"], false);
        assert!(!result.to_string().contains("work limit exceeded"), "{path}: {result}");
    }
}

const REFERENCE_MODELS: &str = r#"from typing import ClassVar
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, registry, relationship
from sqlalchemy import DateTime, String
from sqlalchemy.dialects.postgresql import JSONB
from datetime import datetime
PREFIX = "item"
SUFFIX = 21
LENGTH = len(PREFIX) + 1 + SUFFIX
orm = registry(type_annotation_map={datetime: DateTime(timezone=True), dict: JSONB(none_as_null=True)})
class Base(DeclarativeBase):
    registry: ClassVar = orm
class Label(Base):
    __tablename__ = 'labels'
    id: Mapped[int]
    key: Mapped[str]
class Link(Base):
    __tablename__ = 'links'
    id: Mapped[int]
class Other(Base):
    __tablename__ = 'other'
    id: Mapped[int]
    public_id: Mapped[str] = mapped_column(String(LENGTH))
    data: Mapped[dict] = mapped_column(JSONB)
    labels: Mapped[list[Label]] = relationship(secondary="links", lazy="raise", viewonly=True, order_by=Label.key)
"#;

#[test]
fn table_references_resolve_registry_scalars_and_inferred_relationships() {
    let dir = table_retirement(Orm::SqlAlchemy);
    write(dir.path(), "app/models.py", REFERENCE_MODELS);
    commit(dir.path());
    table_check(&dir, &["--diff", "origin/main"], true);
    write(
        dir.path(),
        "app/models.py",
        &REFERENCE_MODELS
            .replace(
                "key: Mapped[str]",
                "aliased_key: Mapped[str] = mapped_column('key')",
            )
            .replace("order_by=Label.key", "order_by=Label.aliased_key"),
    );
    commit(dir.path());
    table_check(&dir, &["--diff", "origin/main"], true);
    for (name, source, reason) in [
        (
            "target",
            REFERENCE_MODELS.replace("'labels'", "'jobs'"),
            "still",
        ),
        (
            "secondary",
            REFERENCE_MODELS
                .replace("'links'", "'jobs'")
                .replace("secondary=\"links\"", "secondary=\"jobs\""),
            "still",
        ),
        (
            "missing-secondary",
            REFERENCE_MODELS.replace("secondary=\"links\"", "secondary=\"missing\""),
            "relationship",
        ),
        (
            "unknown-option",
            REFERENCE_MODELS.replace("lazy=\"raise\"", "lazy=choose()"),
            "eager attribute expression",
        ),
        (
            "callback",
            REFERENCE_MODELS.replace("order_by=Label.key", "order_by=lambda: Label.key"),
            "eager attribute expression",
        ),
        (
            "third-owner",
            REFERENCE_MODELS.replace("order_by=Label.key", "order_by=Link.id"),
            "relationship",
        ),
        (
            "missing-column",
            REFERENCE_MODELS.replace("order_by=Label.key", "order_by=Label.unknown"),
            "relationship",
        ),
        (
            "physical-name-collision",
            REFERENCE_MODELS.replace("key: Mapped[str]", "aliased_key: Mapped[str] = mapped_column('key')\n    key: ClassVar[str] = 'plain metadata'"),
            "relationship",
        ),
        (
            "unknown-target",
            REFERENCE_MODELS.replace("list[Label]", "list[Missing]"),
            "relationship",
        ),
        (
            "nested-registry-fk",
            REFERENCE_MODELS
                .replace("JSONB(none_as_null=True)", "ForeignKey('jobs.id')")
                .replace("DateTime, String", "DateTime, String, ForeignKey"),
            "unresolved",
        ),
        (
            "registry-annotation",
            REFERENCE_MODELS.replace("registry: ClassVar = orm", "registry: install_mapping() = orm"),
            "unresolved",
        ),
        (
            "dynamic-scalar",
            REFERENCE_MODELS.replace("String(LENGTH)", "String(len(factory()))"),
            "eager attribute expression",
        ),
    ] {
        write(dir.path(), "app/models.py", &source);
        commit(dir.path());
        let result = table_check(&dir, &["--diff", "origin/main"], false);
        assert!(result.to_string().contains(reason), "{name}: {result}");
    }
}

#[test]
fn unresolved_endpoint_relationship_does_not_block_unrelated_column_evidence() {
    let source = REFERENCE_MODELS.replace(
        "secondary=\"links\", lazy=\"raise\", viewonly=True, order_by=Label.key",
        "'Label', back_populates='other'",
    );
    let dir = repo(Orm::SqlAlchemy, &source);
    write(
        dir.path(),
        Orm::SqlAlchemy.migration(),
        "from alembic import op\ndef upgrade():\n    op.drop_column('other', 'retired')\n",
    );
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], true);
}

#[test]
fn eager_class_annotations_cannot_authorize_column_retirement() {
    let prefix = "from typing import ClassVar\nfrom sqlalchemy.orm import registry, relationship, column_property\nfrom sqlalchemy import Column, Integer, String\norm = registry()\ndef install_mapping():\n    return str\n";
    let control = format!("{prefix}{}\nclass Other(Base):\n    __tablename__ = 'other'\n    __allow_unmapped__ = True\n    label: str = 'literal'\n    id: Mapped[int] = mapped_column(primary_key=True)\n", Orm::SqlAlchemy.model(""));
    let dir = repo(Orm::SqlAlchemy, &control);
    add_drop(&dir, Orm::SqlAlchemy, "retired");
    commit(dir.path());
    check(&dir, &["--diff", "origin/main"], true);
    for (body, reason) in [
        ("Alias = Job\n    Alias.__table__.name = 'retired'", "required class binding is mutated"),
        ("table = Job.__table__\n    table.name = 'retired'", "required class binding is mutated"),
        ("Job .__table__.name = 'retired'", "required class binding is mutated"),
        ("(Job).__table__.name = 'retired'", "required class binding is mutated"),
        ("(Job.__table__).name = 'retired'", "required class binding is mutated"),
        ("(Job.__tablename__, other) = ('retired', 0)", "required class binding is mutated"),
        ("globals ()['Job'].__table__.name = 'retired'", "dynamic module namespace"),
        ("from builtins import setattr as set_attribute\n    set_attribute(Job.__table__, 'name', 'retired')", "unsupported explicit ORM declaration"),
        ("setter = setattr\n    setter(Job.__table__, 'name', 'retired')", "explicit namespace mutation"),
        ("from sqlalchemy import Table as HiddenTable\n    HiddenTable('jobs', metadata)", "unsupported explicit ORM declaration"),
        ("orm.map_imperatively(Job, table)", "unsupported explicit ORM declaration"),
    ] {
        let source = format!("{control}\ndef helper():\n    {body}\n");
        let dir = repo(Orm::SqlAlchemy, &source);
        add_drop(&dir, Orm::SqlAlchemy, "retired");
        commit(dir.path());
        let result = check(&dir, &["--diff", "origin/main"], false);
        assert!(result.to_string().contains(reason), "{result}");
    }
    let source = format!("{control}\nsetter = setattr\nclass Deferred(Base):\n    __tablename__ = 'deferred'\n    id: Mapped[int] = mapped_column(default=lambda: setter(Job.__table__, 'name', 'retired'))\n");
    let dir = repo(Orm::SqlAlchemy, &source);
    add_drop(&dir, Orm::SqlAlchemy, "retired");
    commit(dir.path());
    let result = check(&dir, &["--diff", "origin/main"], false);
    assert!(
        result
            .to_string()
            .contains("unsupported explicit ORM declaration"),
        "{result}"
    );
    for owner in [
        "class Other(Base):",
        "class Other(Base):\n    __abstract__ = True",
        "class Other:",
    ] {
        for declaration in [
            "registry: ClassVar[install_mapping()] = orm",
            "value: ClassVar[install_mapping()] = mapped_column()",
            "__tablename__: ClassVar[install_mapping()] = 'other'",
            "__allow_unmapped__ = True\n    value: install_mapping() = 0",
            "registry: ClassVar = install_mapping()",
            "value: ClassVar[int] = install_mapping()",
            "value = install_mapping()",
            "'doc', install_mapping()",
            "__labels__: ClassVar[int] = install_mapping()",
            "value: Mapped[int] = mapped_column(default=install_mapping())",
            "value: Mapped[int] = mapped_column(String(install_mapping()))",
            "value = mapped_column(CustomType)",
            "__doc__ = install_mapping()",
            "__table_args__ = {'comment': install_mapping()}",
            "__table_args__ = (install_mapping(),)",
            "__abstract__ = install_mapping()",
            "value = relationship(install_mapping())",
            "value = column_property(install_mapping())",
            "value: Alias34 = 0",
            "__allow_unmapped__ = True\n    value: \"install_mapping()\" = 0",
            "value: Mapped[\"install_mapping()\"]",
            "__allow_unmapped__ = True\n    value: Forward = 0",
            "value: Mapped[Forward]",
            "class Meta:\n        ordering = install_mapping()",
            "class Meta:\n        ordering: install_mapping() = []",
            "class Meta:\n        class Inner:\n            value = install_mapping()",
        ] {
            let aliases = if declaration.contains("Forward") {
                String::from("Forward = \"install_mapping()\"\n")
            } else if declaration.contains("Alias34") {
                (1..=34).fold(String::from("Alias0 = int\n"), |mut source, n| {
                    source.push_str(&format!("Alias{n} = Alias{}\n", n - 1));
                    source
                })
            } else {
                String::new()
            };
            let custom_type = if declaration == "value = mapped_column(CustomType)" {
                "from sqlalchemy.types import TypeDecorator\nclass CustomType(TypeDecorator):\n    impl = Integer\n    def __init__(self):\n        install_mapping()\n        super().__init__()\n"
            } else {
                ""
            };
            let source = format!("{prefix}{aliases}{custom_type}{}\n{owner}\n    __tablename__ = 'other'\n    id: Mapped[int] = mapped_column(primary_key=True)\n    {declaration}\n", Orm::SqlAlchemy.model(""));
            let dir = repo(Orm::SqlAlchemy, &source);
            add_drop(&dir, Orm::SqlAlchemy, "retired");
            commit(dir.path());
            let result = check(&dir, &["--diff", "origin/main"], false);
            assert!(
                result
                    .to_string()
                    .contains(if declaration.starts_with("'doc',") {
                        "unsupported class-body construct or decorator"
                    } else if declaration.contains("Forward") {
                        "executable forward annotation is unresolved"
                    } else if declaration.contains("class Inner") {
                        "dynamic nested Meta class"
                    } else if declaration.contains("def ordering") {
                        "executable function signature"
                    } else {
                        "eager class expression is unresolved"
                    }),
                "{owner} {declaration}: {result}"
            );
            assert!(
                !result.to_string().contains("work limit exceeded"),
                "{owner} {declaration}: {result}"
            );
        }
    }
    for binding in [
        "__path__ = []",
        "__path__: list = []",
        "__getattr__ = str",
        "__getattr__: object = str",
    ] {
        let source = format!("{binding}\n{}", Orm::SqlAlchemy.model(""));
        let dir = repo(Orm::SqlAlchemy, &source);
        add_drop(&dir, Orm::SqlAlchemy, "retired");
        commit(dir.path());
        let result = check(&dir, &["--diff", "origin/main"], false);
        assert!(
            result.to_string().contains("dynamic module namespace"),
            "{binding}: {result}"
        );
    }
    for base in [
        "Base, install_mapping()",
        "Base, metaclass=install_mapping()",
        "Base, flag=install_mapping()",
    ] {
        let source = format!(
            "{prefix}{}\nclass Other({base}):\n    __tablename__ = 'other'\n    id: Mapped[int]\n",
            Orm::SqlAlchemy.model("")
        );
        let dir = repo(Orm::SqlAlchemy, &source);
        add_drop(&dir, Orm::SqlAlchemy, "retired");
        commit(dir.path());
        let result = check(&dir, &["--diff", "origin/main"], false);
        assert!(
            result.to_string().contains("eager base expression"),
            "{base}: {result}"
        );
        assert!(
            !result.to_string().contains("work limit exceeded"),
            "{base}: {result}"
        );
    }
    for declaration in [
        "ordering = install_mapping()",
        "ordering: install_mapping() = []",
    ] {
        let source = format!("from tortoise import models, fields\ndef install_mapping():\n    return type('Hidden', (models.Model,), {{'retired': fields.IntField()}})\nclass Job(models.Model):\n    class Meta:\n        table = 'jobs'\nclass Abstract(models.Model):\n    class Meta:\n        abstract = True\n        {declaration}\n");
        let dir = repo(Orm::Tortoise, &source);
        add_drop(&dir, Orm::Tortoise, "retired");
        commit(dir.path());
        let result = check(&dir, &["--diff", "origin/main"], false);
        assert!(
            result.to_string().contains("eager Meta expression"),
            "{result}"
        );
    }
}
