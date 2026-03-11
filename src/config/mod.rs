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
#[derive(Debug, Clone)]
pub struct Config {
    /// Rules to select (if empty, all rules are selected).
    pub select: HashSet<String>,
    /// Rules to ignore.
    pub ignore: HashSet<String>,
    /// Treat warnings as errors.
    pub warnings_as_errors: bool,
    /// File patterns to exclude from linting.
    pub exclude: Vec<String>,
    /// For R008: file patterns that are disallowed to change alongside migrations.
    pub disallowed_file_patterns: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            select: HashSet::new(),
            ignore: HashSet::new(),
            warnings_as_errors: false,
            exclude: vec![],
            disallowed_file_patterns: vec![
                // Default patterns for R008
                "*.py".to_string(),
            ],
        }
    }
}

impl Config {
    /// Create a new config with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a rule is enabled.
    pub fn is_rule_enabled(&self, rule_id: &str) -> bool {
        // If select is empty, all rules are enabled by default
        let selected = self.select.is_empty() || self.select.contains(rule_id);
        let ignored = self.ignore.contains(rule_id);
        selected && !ignored
    }

    /// Load configuration from a directory, searching for config files.
    pub fn load_from_directory(dir: &Path) -> Result<Self> {
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

        Ok(config)
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
        if let Some(patterns) = other.disallowed_file_patterns {
            self.disallowed_file_patterns = patterns;
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
pub struct FileConfig {
    /// Rules to select.
    pub select: Option<Vec<String>>,
    /// Rules to ignore.
    pub ignore: Option<Vec<String>>,
    /// Treat warnings as errors.
    pub warnings_as_errors: Option<bool>,
    /// File patterns to exclude.
    pub exclude: Option<Vec<String>>,
    /// For R008: disallowed file patterns.
    pub disallowed_file_patterns: Option<Vec<String>>,
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
}
