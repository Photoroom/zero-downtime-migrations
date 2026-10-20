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
                assert!(result
                    .to_string()
                    .contains("checked revision: column signed_manifest is still mapped"));
            }
        }
    }
}
