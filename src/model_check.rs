//! Historical ORM evidence for R005. Never imports or executes application code.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast::TableIdentity;
use crate::config::{FileConfig, ModelSource};
use crate::discovery::MigrationFramework;
use crate::git::{DiffSource, GitRepo, GitSnapshot};
use crate::models::{ModelFile, ModelIndex};

/// Evidence built once from immutable Git objects, using base-established scopes.
pub struct ModelEvidence {
    base: Result<ModelIndex, String>,
    target: Result<ModelIndex, String>,
}

impl ModelEvidence {
    /// Build evidence for the same merge-base and source as a migration diff.
    /// Failure is retained as missing evidence, never interpreted as absence.
    pub fn from_git(repo: &GitRepo, base_ref: &str, source: DiffSource) -> Self {
        let snapshots = (|| {
            let base = repo
                .model_snapshot(Some(base_ref), source)
                .map_err(|e| e.to_string())?;
            let scopes = model_sources(repo, &base)?;
            let target = repo
                .model_snapshot(None, source)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((base, target, scopes))
        })();
        match snapshots {
            Ok((base, target, scopes)) => Self {
                base: build_index(repo, &base, &scopes),
                target: build_index(repo, &target, &scopes),
            },
            Err(reason) => Self {
                base: Err(reason.clone()),
                target: Err(reason),
            },
        }
    }

    /// Establish that every supported mapping excludes the exact physical column.
    pub fn column_absence(
        &self,
        framework: MigrationFramework,
        table: &TableIdentity,
        column: &str,
    ) -> Result<(), String> {
        for (snapshot, evidence) in [
            ("merge-base", &self.base),
            ("checked revision", &self.target),
        ] {
            let index = evidence
                .as_ref()
                .map_err(|reason| format!("{snapshot}: {reason}"))?;
            index
                .column_absence(framework, table, column)
                .map_err(|reason| format!("{snapshot}: {reason}"))?;
        }
        Ok(())
    }
}

fn model_sources(repo: &GitRepo, base: &GitSnapshot) -> Result<Vec<ModelSource>, String> {
    let mut scopes = Vec::new();
    for name in ["pyproject.toml", "zero-downtime-migrations.toml"] {
        let Some(content) = repo
            .read_snapshot_file(base, Path::new(name))
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        if content.len() > 1024 * 1024 {
            return Err(format!("{name}: model configuration exceeds 1 MiB"));
        }
        let mut value: toml::Value =
            toml::from_str(&content).map_err(|e| format!("{name}: {e}"))?;
        if name == "pyproject.toml" {
            let Some(zdm) = value.get("tool").and_then(|tool| tool.get("zdm")) else {
                continue;
            };
            value = zdm.clone();
        }
        let config: FileConfig = value.try_into().map_err(|e| format!("{name}: {e}"))?;
        if let Some(model_sources) = config.model_sources {
            scopes = model_sources;
        }
    }
    if scopes.is_empty() {
        return Err(
            "no model-sources configured at the merge-base; adopt source coverage in an earlier PR"
                .into(),
        );
    }
    for scope in &scopes {
        scope.validate()?;
    }
    Ok(scopes)
}

fn build_index(
    repo: &GitRepo,
    snapshot: &GitSnapshot,
    scopes: &[ModelSource],
) -> Result<ModelIndex, String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut bytes = 0usize;
    for scope in scopes {
        for directory in &scope.paths {
            let directory: PathBuf = directory
                .components()
                .filter(|part| matches!(part, std::path::Component::Normal(_)))
                .collect();
            let mut found = false;
            for path in snapshot.entries.keys() {
                if !directory.as_os_str().is_empty()
                    && path != &directory
                    && !path.starts_with(&directory)
                {
                    continue;
                }
                // A symlink/submodule at a configured root is not an empty scope.
                let mode = snapshot.entries[path].1;
                if mode & 0o170000 != 0o100000 {
                    return Err(format!(
                        "non-regular source '{}' prevents complete model coverage",
                        path.display()
                    ));
                }
                if path.extension().is_none_or(|extension| extension != "py") {
                    continue;
                }
                found = true;
                if !seen.insert((scope.orm, path.clone())) {
                    continue;
                }
                if files.len() >= 10_000 {
                    return Err("model source coverage exceeds 10000 files".into());
                }
                let source = repo
                    .read_snapshot_file(snapshot, path)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("missing model source '{}'", path.display()))?;
                bytes += source.len();
                if bytes > 100 * 1024 * 1024 {
                    return Err("model source coverage exceeds 100 MiB".into());
                }
                files.push(ModelFile {
                    path: path.clone(),
                    orm: scope.orm,
                    source,
                });
            }
            if !found {
                return Err(format!(
                    "model source '{}' contains no Python files",
                    directory.display()
                ));
            }
        }
    }
    Ok(ModelIndex::build(files))
}
