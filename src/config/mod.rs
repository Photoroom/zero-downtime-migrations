//! Configuration parsing and management.
//!
//! Supports:
//! - `pyproject.toml` with `[tool.zdm]` section
//! - Standalone `zero-downtime-migrations.toml`
//! - CLI flag overrides
//!
//! Precedence (highest to lowest):
//! 1. CLI flags
//! 2. `zero-downtime-migrations.toml`
//! 3. `pyproject.toml [tool.zdm]`
//! 4. Default values

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Configuration for zdm.
///
/// `exclude` and `allowed_file_patterns` are validated at config
/// load (`Config::load_from_directory` → `validate_glob_patterns`).
/// Library consumers that mutate these fields directly can call
/// `validate_glob_patterns` to surface invalid glob syntax before
/// running discovery or changeset rules.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Config {
    /// Rules to select (if empty, all rules are selected).
    pub select: HashSet<String>,
    /// Rules to ignore.
    pub ignore: HashSet<String>,
    /// Treat warnings as errors.
    pub warnings_as_errors: bool,
    /// File patterns to exclude from linting.
    pub exclude: Vec<String>,
    /// For R008: file patterns allowed to change alongside
    /// migrations. Files NOT matching these patterns will trigger a
    /// warning.
    pub allowed_file_patterns: Vec<String>,
}

impl Config {
    /// Check if a rule is enabled.
    pub fn is_rule_enabled(&self, rule_id: &str) -> bool {
        // If select is empty, all rules are enabled by default
        let selected = self.select.is_empty() || self.select.contains(rule_id);
        let ignored = self.ignore.contains(rule_id);
        selected && !ignored
    }

    /// Load configuration from a directory. If `dir` is inside a trusted
    /// git repository, walk up within that repository until a config file is
    /// found or the repository root is reached. Without a `.git` ancestor,
    /// only `dir` itself is checked so zdm does not accidentally adopt
    /// config from a shared parent directory.
    ///
    /// At each level, standalone config and `pyproject.toml [tool.zdm]`
    /// are tried (standalone wins where both are present). A
    /// `pyproject.toml` without `[tool.zdm]` is ignored so unrelated
    /// tool config in a subdirectory does not shadow repo-level zdm
    /// config. Multi-level merging is not performed.
    ///
    /// Glob patterns in `exclude` and `allowed_file_patterns` are
    /// validated at load time; an invalid pattern surfaces as
    /// `Error::InvalidGlobPattern` instead of being silently dropped
    /// at the match site.
    pub fn load_from_directory(dir: &Path) -> Result<Self> {
        let config_dir = Self::find_config_dir(dir)?;
        let search_dir = config_dir.as_deref().unwrap_or(dir);
        Self::load_from_single_dir(search_dir)
    }

    /// Load configuration from exactly `dir`, without walking upward.
    ///
    /// Standalone config still overrides `pyproject.toml [tool.zdm]` within
    /// that one directory, and glob patterns are still validated.
    pub fn load_from_exact_directory(dir: &Path) -> Result<Self> {
        Self::load_from_single_dir(dir)
    }

    /// Walk upward from `start` looking for the first directory that
    /// contains `zero-downtime-migrations.toml` or a `pyproject.toml`
    /// with `[tool.zdm]`. Stop at any directory that holds a `.git`
    /// entry (the repository root — a config above the repo would
    /// belong to a parent project and is not ours to read).
    ///
    /// The walk only escapes `start` when a `.git` ancestor actually
    /// exists somewhere above. Without that anchor — for example,
    /// when zdm runs in an unpacked tarball under `/tmp/build/foo`
    /// or a non-git CI workspace — the walk would otherwise climb
    /// all the way to `/` and pick up a world-writable
    /// `/tmp/zero-downtime-migrations.toml`. Restricting upward
    /// movement to within a confirmed repo closes that escalation
    /// path.
    ///
    /// Returns `None` if no config file is found within those
    /// bounds.
    fn find_config_dir(start: &Path) -> Result<Option<std::path::PathBuf>> {
        // Config in `start` wins; if `start` is itself the repo root we never
        // climb (a structural guard that also covers Windows, where
        // `anchor_dir_is_trusted` is a no-op); otherwise walk up only inside a
        // trusted git repo, stopping at the first config found or at the anchor.
        if has_zdm_config_file(start)? {
            return Ok(Some(start.to_path_buf()));
        }
        if start.join(".git").exists() {
            return Ok(None);
        }

        let Some(anchor) = trusted_git_anchor_strictly_above(start) else {
            return Ok(None);
        };

        let mut current = start;
        while let Some(parent) = current.parent() {
            current = parent;
            if has_zdm_config_file(current)? {
                return Ok(Some(current.to_path_buf()));
            }
            if current == anchor {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Load whichever config files exist in a single directory,
    /// merging them with the documented precedence (standalone
    /// overrides pyproject) and validating glob patterns.
    fn load_from_single_dir(dir: &Path) -> Result<Self> {
        let mut config = Config::default();

        // Try pyproject.toml first (lowest precedence of file configs)
        let pyproject_path = dir.join("pyproject.toml");
        if pyproject_path.exists() {
            if let Some(file_config) = Self::load_pyproject(&pyproject_path)? {
                config.merge(file_config);
            }
        }

        // Try standalone config (higher precedence)
        let standalone_path = dir.join("zero-downtime-migrations.toml");
        if standalone_path.exists() {
            let file_config = Self::load_standalone(&standalone_path)?;
            config.merge(file_config);
        }

        config.validate_glob_patterns()?;
        Ok(config)
    }

    /// Compile every glob pattern in this config to surface syntax errors
    /// early. The compiled patterns are discarded — call sites recompile
    /// on demand, but the compilation is guaranteed to succeed after this.
    pub fn validate_glob_patterns(&self) -> Result<()> {
        for pattern in self.exclude.iter().chain(self.allowed_file_patterns.iter()) {
            glob::Pattern::new(pattern).map_err(|e| Error::InvalidGlobPattern {
                pattern: pattern.clone(),
                message: e.to_string(),
            })?;
        }
        Ok(())
    }

    /// Load from pyproject.toml.
    fn load_pyproject(path: &Path) -> Result<Option<FileConfig>> {
        let content = std::fs::read_to_string(path).map_err(|e| Error::file_read(path, e))?;

        let pyproject: PyProjectToml =
            toml::from_str(&content).map_err(|e| Error::config_parse_error(path, e))?;

        Ok(pyproject.tool.and_then(|t| t.zdm))
    }

    /// Load from standalone zero-downtime-migrations.toml.
    fn load_standalone(path: &Path) -> Result<FileConfig> {
        let content = std::fs::read_to_string(path).map_err(|e| Error::file_read(path, e))?;

        toml::from_str(&content).map_err(|e| Error::config_parse_error(path, e))
    }

    /// Merge another config into this one (other takes precedence).
    pub fn merge(&mut self, other: FileConfig) {
        if let Some(select) = other.select {
            self.select = select.into_iter().collect();
        }
        if let Some(ignore) = other.ignore {
            self.ignore = ignore.into_iter().collect();
        }
        if let Some(warnings_as_errors) = other.warnings_as_errors {
            self.warnings_as_errors = warnings_as_errors;
        }
        if let Some(exclude) = other.exclude {
            self.exclude = exclude;
        }
        if let Some(patterns) = other.allowed_file_patterns {
            self.allowed_file_patterns = patterns;
        }
    }

    /// Apply CLI overrides (highest precedence).
    pub fn apply_cli_overrides(
        &mut self,
        select: Option<Vec<String>>,
        ignore: Option<Vec<String>>,
        warnings_as_errors: bool,
    ) {
        if let Some(select) = select {
            self.select = select.into_iter().collect();
        }
        if let Some(ignore) = ignore {
            // CLI ignore is additive to config ignore
            self.ignore.extend(ignore);
        }
        if warnings_as_errors {
            self.warnings_as_errors = true;
        }
    }
}

/// `true` iff a zdm config file lives directly in `dir`.
///
/// A bare `pyproject.toml` for another tool does not count; otherwise it
/// would shadow a repo-level zdm config while contributing only defaults.
fn has_zdm_config_file(dir: &Path) -> Result<bool> {
    if dir.join("zero-downtime-migrations.toml").exists() {
        return Ok(true);
    }
    let pyproject_path = dir.join("pyproject.toml");
    if !pyproject_path.exists() {
        return Ok(false);
    }
    Config::load_pyproject(&pyproject_path).map(|config| config.is_some())
}

/// The closest directory *strictly above* `start` that holds a `.git` entry
/// AND passes [`anchor_dir_is_trusted`]. `None` means the walk-up is not
/// authorised (not in a repo, or the only visible `.git` was planted by
/// another user). `.git` may be a directory or a worktree/submodule pointer
/// file; both count.
fn trusted_git_anchor_strictly_above(start: &Path) -> Option<std::path::PathBuf> {
    let mut probe = start;
    while let Some(parent) = probe.parent() {
        if parent.join(".git").exists() && anchor_dir_is_trusted(parent) {
            return Some(parent.to_path_buf());
        }
        probe = parent;
    }
    None
}

/// Rejects a `.git` ancestor an attacker could plant in a shared parent
/// (classic `/tmp` sticky-bit scenario) before we trust it as the walk-up
/// anchor. On Unix: `true` iff `dir` is not group/other-writable and is
/// owned by the effective uid (or root). On Windows: always `true` —
/// ACL-based ownership has no simple uid check yet (TODO).
#[cfg(unix)]
fn anchor_dir_is_trusted(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(dir) {
        Ok(meta) => {
            // SAFETY: `geteuid` is a libc call with no preconditions.
            let euid = unsafe { libc::geteuid() };
            let group_or_other_writable = meta.mode() & 0o022 != 0;
            !group_or_other_writable && (meta.uid() == euid || meta.uid() == 0)
        }
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn anchor_dir_is_trusted(_dir: &Path) -> bool {
    true
}

/// pyproject.toml structure.
#[derive(Debug, Deserialize)]
struct PyProjectToml {
    tool: Option<Tool>,
}

#[derive(Debug, Deserialize)]
struct Tool {
    zdm: Option<FileConfig>,
}

/// Configuration from a file (all fields optional for merging).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub struct FileConfig {
    /// Rules to select.
    pub select: Option<Vec<String>>,
    /// Rules to ignore.
    pub ignore: Option<Vec<String>>,
    /// Treat warnings as errors.
    pub warnings_as_errors: Option<bool>,
    /// File patterns to exclude.
    pub exclude: Option<Vec<String>>,
    /// For R008: allowed file patterns.
    pub allowed_file_patterns: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(config.select.is_empty());
        assert!(config.ignore.is_empty());
        assert!(!config.warnings_as_errors);
    }

    #[test]
    fn test_is_rule_enabled_default() {
        let config = Config::default();
        // All rules enabled by default
        assert!(config.is_rule_enabled("R001"));
        assert!(config.is_rule_enabled("R017"));
    }

    #[test]
    fn test_is_rule_enabled_with_select() {
        let mut config = Config::default();
        config.select.insert("R001".to_string());
        config.select.insert("R002".to_string());

        assert!(config.is_rule_enabled("R001"));
        assert!(config.is_rule_enabled("R002"));
        assert!(!config.is_rule_enabled("R003"));
    }

    #[test]
    fn test_is_rule_enabled_with_ignore() {
        let mut config = Config::default();
        config.ignore.insert("R001".to_string());

        assert!(!config.is_rule_enabled("R001"));
        assert!(config.is_rule_enabled("R002"));
    }

    #[test]
    fn test_is_rule_enabled_select_and_ignore() {
        let mut config = Config::default();
        config.select.insert("R001".to_string());
        config.select.insert("R002".to_string());
        config.ignore.insert("R001".to_string());

        // Ignore takes precedence
        assert!(!config.is_rule_enabled("R001"));
        assert!(config.is_rule_enabled("R002"));
    }

    #[test]
    fn test_load_pyproject_toml() {
        let temp = TempDir::new().unwrap();
        let pyproject_path = temp.path().join("pyproject.toml");

        fs::write(
            &pyproject_path,
            r#"
[tool.zdm]
select = ["R001", "R002"]
ignore = ["R003"]
warnings-as-errors = true
"#,
        )
        .unwrap();

        let config = Config::load_from_directory(temp.path()).unwrap();

        assert!(config.select.contains("R001"));
        assert!(config.select.contains("R002"));
        assert!(config.ignore.contains("R003"));
        assert!(config.warnings_as_errors);
    }

    #[test]
    fn test_load_standalone_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zero-downtime-migrations.toml");

        fs::write(
            &config_path,
            r#"
select = ["R001"]
warnings-as-errors = true
"#,
        )
        .unwrap();

        let config = Config::load_from_directory(temp.path()).unwrap();

        assert!(config.select.contains("R001"));
        assert!(config.warnings_as_errors);
    }

    #[test]
    fn test_config_precedence() {
        let temp = TempDir::new().unwrap();

        // pyproject.toml with some settings
        fs::write(
            temp.path().join("pyproject.toml"),
            r#"
[tool.zdm]
select = ["R001", "R002"]
warnings-as-errors = false
"#,
        )
        .unwrap();

        // standalone config overrides
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"
select = ["R003"]
warnings-as-errors = true
"#,
        )
        .unwrap();

        let config = Config::load_from_directory(temp.path()).unwrap();

        // Standalone takes precedence
        assert!(!config.select.contains("R001"));
        assert!(config.select.contains("R003"));
        assert!(config.warnings_as_errors);
    }

    #[test]
    fn test_cli_overrides() {
        let temp = TempDir::new().unwrap();

        fs::write(
            temp.path().join("pyproject.toml"),
            r#"
[tool.zdm]
select = ["R001"]
ignore = ["R002"]
"#,
        )
        .unwrap();

        let mut config = Config::load_from_directory(temp.path()).unwrap();
        config.apply_cli_overrides(
            Some(vec!["R005".to_string()]),
            Some(vec!["R006".to_string()]),
            true,
        );

        // CLI select replaces file select
        assert!(!config.select.contains("R001"));
        assert!(config.select.contains("R005"));

        // CLI ignore is additive
        assert!(config.ignore.contains("R002"));
        assert!(config.ignore.contains("R006"));

        assert!(config.warnings_as_errors);
    }

    #[test]
    fn test_no_config_files() {
        let temp = TempDir::new().unwrap();
        let config = Config::load_from_directory(temp.path()).unwrap();

        // Should return defaults
        assert!(config.select.is_empty());
        assert!(config.ignore.is_empty());
        assert!(!config.warnings_as_errors);
    }

    #[test]
    fn test_pyproject_without_zdm_section() {
        let temp = TempDir::new().unwrap();

        fs::write(
            temp.path().join("pyproject.toml"),
            r#"
[tool.black]
line-length = 88
"#,
        )
        .unwrap();

        let config = Config::load_from_directory(temp.path()).unwrap();

        // Should return defaults
        assert!(config.select.is_empty());
    }

    #[test]
    fn test_exclude_patterns() {
        let temp = TempDir::new().unwrap();

        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"
exclude = ["**/test_migrations/**", "**/fixtures/**"]
"#,
        )
        .unwrap();

        let config = Config::load_from_directory(temp.path()).unwrap();

        assert_eq!(config.exclude.len(), 2);
        assert!(config
            .exclude
            .contains(&"**/test_migrations/**".to_string()));
    }

    #[test]
    fn test_invalid_glob_pattern_in_exclude_errors() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            // `[` opens a character class that is never closed — a real
            // syntactic error in the glob crate.
            r#"exclude = ["src/[unclosed"]"#,
        )
        .unwrap();

        match Config::load_from_directory(temp.path()) {
            Err(Error::InvalidGlobPattern { pattern, .. }) => {
                assert_eq!(pattern, "src/[unclosed");
            }
            other => panic!("expected InvalidGlobPattern, got {other:?}"),
        }
    }

    #[test]
    fn test_invalid_glob_pattern_in_allowed_files_errors() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"allowed-file-patterns = ["models.py", "*.[broken"]"#,
        )
        .unwrap();

        match Config::load_from_directory(temp.path()) {
            Err(Error::InvalidGlobPattern { pattern, .. }) => {
                assert_eq!(pattern, "*.[broken");
            }
            other => panic!("expected InvalidGlobPattern, got {other:?}"),
        }
    }

    #[test]
    fn test_load_walks_up_to_find_config() {
        // Drop a config in the repo root, then invoke load from a
        // deeply nested subdirectory. The walk-up should pick up
        // the root config so developers don't have to cd to the
        // project root. The walk only escapes `start` when a
        // `.git` ancestor exists, so plant a sentinel.
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".git")).unwrap();
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R001"]"#,
        )
        .unwrap();

        let nested = temp.path().join("apps/myapp/migrations");
        fs::create_dir_all(&nested).unwrap();

        let config = Config::load_from_directory(&nested).unwrap();
        assert!(config.ignore.contains("R001"));
    }

    #[test]
    fn test_walk_stops_at_git_directory() {
        // A `.git` entry marks the project root. A config above the
        // repo would belong to a parent project and is not ours to
        // read — the walk must stop and fall back to defaults.
        let temp = TempDir::new().unwrap();
        // Parent-of-repo config that should NOT be picked up.
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R002"]"#,
        )
        .unwrap();

        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("apps/myapp");
        fs::create_dir_all(&nested).unwrap();

        let config = Config::load_from_directory(&nested).unwrap();
        assert!(
            !config.ignore.contains("R002"),
            "config above the .git directory should not be loaded",
        );
    }

    #[test]
    fn test_walk_stops_at_git_file_worktree() {
        // In a git worktree the `.git` entry is a FILE (containing a
        // gitdir pointer), not a directory. The walk should treat
        // either form as the repo boundary.
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R003"]"#,
        )
        .unwrap();

        let worktree = temp.path().join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join(".git"), "gitdir: /some/git/dir").unwrap();
        let nested = worktree.join("apps/myapp");
        fs::create_dir_all(&nested).unwrap();

        let config = Config::load_from_directory(&nested).unwrap();
        assert!(!config.ignore.contains("R003"));
    }

    #[test]
    fn test_walk_picks_nearest_config() {
        // When configs exist at multiple levels, the nearest one
        // wins; multi-level merging would surprise users with
        // `[tool.zdm]` blocks scattered across nested pyprojects.
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".git")).unwrap();
        // Outer config: ignores R001.
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R001"]"#,
        )
        .unwrap();

        // Inner config: ignores R002 only. R001 must not bleed
        // through from the outer config.
        let inner = temp.path().join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(
            inner.join("zero-downtime-migrations.toml"),
            r#"ignore = ["R002"]"#,
        )
        .unwrap();

        let nested = inner.join("apps/myapp");
        fs::create_dir_all(&nested).unwrap();

        let config = Config::load_from_directory(&nested).unwrap();
        assert!(config.ignore.contains("R002"));
        assert!(
            !config.ignore.contains("R001"),
            "outer config should not bleed through when the inner config exists",
        );
    }

    #[test]
    fn test_nested_pyproject_without_zdm_does_not_shadow_parent_config() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".git")).unwrap();
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R001"]"#,
        )
        .unwrap();

        let nested = temp.path().join("apps/myapp");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("pyproject.toml"),
            r#"
[tool.black]
line-length = 88
"#,
        )
        .unwrap();

        let config = Config::load_from_directory(&nested).unwrap();
        assert!(
            config.ignore.contains("R001"),
            "pyproject.toml without [tool.zdm] should not hide parent zdm config",
        );
    }

    #[test]
    fn test_load_from_exact_directory_does_not_walk_up() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R001"]"#,
        )
        .unwrap();
        let nested = temp.path().join("apps/myapp");
        fs::create_dir_all(&nested).unwrap();

        let config = Config::load_from_exact_directory(&nested).unwrap();
        assert!(config.ignore.is_empty());
    }

    #[test]
    fn test_walk_does_not_escape_when_git_is_in_start_dir() {
        // SECURITY: when `start` itself holds `.git`, the walk must
        // not climb into `start.parent()` — start IS the repo root,
        // and a world-writable config in the parent (e.g.
        // `/tmp/zero-downtime-migrations.toml`) must not be adopted
        // as "the project config". The first cut of the `.git`-anchor
        // fix only blocked the no-`.git`-anywhere case; this case
        // round-trips the full silent-escalation path the security
        // reviewer reproduced.
        let temp = TempDir::new().unwrap();
        // Parent-of-repo config that should NOT be picked up:
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["RXXX"]"#,
        )
        .unwrap();

        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        // No config inside `repo` — start IS the repo root.

        let config = Config::load_from_directory(&repo).unwrap();
        assert!(
            !config.ignore.contains("RXXX"),
            "config from parent of repo must not bleed in when start is repo root, got: {:?}",
            config.ignore,
        );
    }

    #[test]
    fn test_walk_does_not_escape_when_no_git_ancestor() {
        // If there is no `.git` ancestor anywhere above `start`, the
        // walk must NOT climb into parent directories — a config
        // sitting in a shared temp dir, a CI workspace's parent, or
        // even `/` could be world-writable and would otherwise be
        // adopted as a legitimate project config.
        let temp = TempDir::new().unwrap();
        // No `.git` anywhere. A config that should NOT be loaded
        // from a sibling/parent directory:
        fs::write(
            temp.path().join("zero-downtime-migrations.toml"),
            r#"ignore = ["R007"]"#,
        )
        .unwrap();
        let nested = temp.path().join("no_git_repo/sub");
        fs::create_dir_all(&nested).unwrap();

        let config = Config::load_from_directory(&nested).unwrap();
        assert!(
            !config.ignore.contains("R007"),
            "parent-of-non-repo config should not be loaded",
        );
        assert!(config.select.is_empty());
        assert!(config.ignore.is_empty());
    }

    #[test]
    fn test_no_config_no_git_returns_defaults() {
        // Both no config and no `.git` anywhere: the loop must
        // terminate by returning `None` from `find_config_dir`
        // rather than walking all the way to the filesystem root.
        let temp = TempDir::new().unwrap();
        let nested = temp.path().join("a/b/c/d");
        fs::create_dir_all(&nested).unwrap();

        let config = Config::load_from_directory(&nested).unwrap();
        assert!(config.select.is_empty());
        assert!(config.ignore.is_empty());
        assert!(config.exclude.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_anchor_dir_is_trusted_accepts_self_owned_dir() {
        // Sanity check: a directory we own — the temp dir we
        // just created — is trusted. Without this, the trust
        // check would reject every legitimate `.git` ancestor in
        // a normal user checkout, breaking the walk-up entirely.
        let temp = TempDir::new().unwrap();
        assert!(
            anchor_dir_is_trusted(temp.path()),
            "a self-created tempdir should be trusted by the uid check",
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_anchor_dir_is_trusted_rejects_nonexistent() {
        // A nonexistent path can't be trusted — defensive false
        // is correct (the walk-up keeps probing parents until it
        // finds a trusted anchor or runs out).
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("does-not-exist");
        assert!(!anchor_dir_is_trusted(&missing));
    }

    #[cfg(unix)]
    #[test]
    fn test_anchor_dir_is_trusted_rejects_world_writable_dir() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("shared");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        assert!(
            !anchor_dir_is_trusted(&dir),
            "world-writable anchors must not authorize config walk-up",
        );
    }
}
