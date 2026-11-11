//! Historical ORM evidence for R005 and R019. Never imports or executes application code.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::TableIdentity;
use crate::config::{FileConfig, ModelSource};
use crate::discovery::MigrationFramework;
use crate::git::{DiffSource, GitRepo, GitSnapshot};
use crate::models::{check_package, ModelFile, ModelIndex, TRUSTED_ROOTS};

/// Evidence built once from immutable Git objects, using base-established scopes.
pub struct ModelEvidence {
    prior: Result<ModelIndex, String>,
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
            let (scopes, witness) = model_sources(repo, &base)?;
            let target = repo
                .model_snapshot(None, source)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((base, target, scopes, witness))
        })();
        match snapshots {
            Ok((base, target, scopes, witness)) => Self {
                prior: witness
                    .ok_or_else(|| {
                        "no model-retirement-ref configured at the merge-base".to_string()
                    })
                    .and_then(|witness| {
                        repo.retirement_snapshot(base_ref, &witness)
                            .map_err(|e| e.to_string())
                    })
                    .and_then(|snapshot| build_index(repo, &snapshot, &scopes)),
                base: build_index(repo, &base, &scopes),
                target: build_index(repo, &target, &scopes),
            },
            Err(reason) => Self {
                prior: Err(reason.clone()),
                base: Err(reason.clone()),
                target: Err(reason),
            },
        }
    }

    /// Require a known historical table and absence before the contraction change.
    pub fn table_absence(
        &self,
        framework: MigrationFramework,
        table: &TableIdentity,
    ) -> Result<(), String> {
        self.prior
            .as_ref()
            .map_err(|reason| format!("historical witness: {reason}"))?
            .table_presence(framework, table)
            .map_err(|reason| format!("historical witness: {reason}"))?;
        for (snapshot, evidence) in [
            ("merge-base", &self.base),
            ("checked revision", &self.target),
        ] {
            evidence
                .as_ref()
                .map_err(|reason| format!("{snapshot}: {reason}"))?
                .table_absence(table)
                .map_err(|reason| format!("{snapshot}: {reason}"))?;
        }
        Ok(())
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

fn model_sources(
    repo: &GitRepo,
    base: &GitSnapshot,
) -> Result<(Vec<ModelSource>, Option<String>), String> {
    let mut scopes = Vec::new();
    let mut witness = None;
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
        if let Some(reference) = config.model_retirement_ref {
            witness = Some(reference);
        }
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
    Ok((scopes, witness))
}

fn opaque_context(module: &Path, opaque: &HashSet<PathBuf>) -> bool {
    module
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .any(|path| opaque.contains(path) || opaque.contains(&path.join("__init__")))
}

fn build_index(
    repo: &GitRepo,
    snapshot: &GitSnapshot,
    scopes: &[ModelSource],
) -> Result<ModelIndex, String> {
    let opaque: HashSet<_> = snapshot
        .entries
        .keys()
        .filter_map(|path| {
            let filename = path.file_name()?.to_str()?;
            let (stem, suffix) = filename.split_once('.')?;
            let extension = suffix.rsplit('.').next()?.to_ascii_lowercase();
            (!stem.is_empty() && matches!(extension.as_str(), "so" | "pyd" | "pyc" | "pyx" | "pyw"))
                .then(|| path.with_file_name(stem))
        })
        .collect();
    for name in TRUSTED_ROOTS {
        if [
            PathBuf::from(format!("{name}.py")),
            PathBuf::from(format!("{name}/__init__.py")),
        ]
        .iter()
        .any(|path| snapshot.entries.contains_key(path))
            || opaque_context(Path::new(name), &opaque)
        {
            return Err(format!("{name}: repository shadows a supported library"));
        }
    }
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
                if opaque_context(&path.with_extension(""), &opaque) {
                    return Err(format!(
                        "{}: opaque model source context is unresolved",
                        path.display()
                    ));
                }
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
    let audited_paths: HashSet<_> = files.iter().map(|file| file.path.clone()).collect();
    let mut dependency_modules = HashSet::new();
    let mut checked_packages = HashSet::new();
    let mut package_parses = HashMap::new();
    let mut dependency = |symbol: &str| {
        let Some((module, _)) = symbol.rsplit_once('.') else {
            return Ok(None);
        };
        if module.split('.').any(|part| {
            part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            return Err("invalid dependency module path".into());
        }
        let path = PathBuf::from(module.replace('.', "/"));
        if opaque_context(&path, &opaque) {
            return Err(format!("{module}: opaque module dependency is unresolved"));
        }
        let candidates = [path.with_extension("py"), path.join("__init__.py")];
        let found: Vec<_> = candidates
            .iter()
            .filter(|path| snapshot.entries.contains_key(*path))
            .collect();
        if found.is_empty() {
            return Ok(None);
        }
        if found.len() != 1 {
            return Err(format!("{module}: ambiguous module/package dependency"));
        }
        let mut read = |path: &Path| -> Result<ModelFile, String> {
            let source = repo
                .read_snapshot_file(snapshot, path)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("missing dependency context '{}'", path.display()))?;
            if seen.insert((crate::config::ModelOrm::Sqlalchemy, path.to_path_buf())) {
                bytes += source.len();
                if seen.len() > 10_000 || bytes > 100 * 1024 * 1024 {
                    return Err("model sources and dependencies exceed evidence limits".into());
                }
            }
            Ok(ModelFile {
                path: path.to_path_buf(),
                orm: crate::config::ModelOrm::Sqlalchemy,
                source,
            })
        };
        let parts: Vec<_> = module.split('.').collect();
        for level in 1..parts.len() {
            if snapshot
                .entries
                .contains_key(&PathBuf::from(parts[..level].join("/")).with_extension("py"))
            {
                return Err(format!(
                    "{module}: ambiguous parent module/package dependency"
                ));
            }
            let parent = PathBuf::from(parts[..level].join("/")).join("__init__.py");
            if checked_packages.insert((parent.clone(), parts[level].to_owned(), symbol.to_owned()))
            {
                let parsed = package_parses.entry(parent.clone()).or_insert_with(|| {
                    crate::parser::ParsedMigration::parse(read(&parent)?.source)
                        .map_err(|error| error.to_string())
                });
                check_package(
                    &parent,
                    parsed.as_ref().map_err(Clone::clone)?,
                    parts[level],
                    symbol,
                )?;
            }
        }
        if !dependency_modules.insert(module.to_owned()) || audited_paths.contains(found[0]) {
            return Ok(None);
        }
        Ok(Some(read(found[0])?))
    };
    for file in &files {
        let module = file
            .path
            .with_extension("")
            .to_string_lossy()
            .replace(['/', '\\'], ".");
        let module = module.strip_suffix(".__init__").unwrap_or(&module);
        dependency(&format!("{module}.__audited_root__"))?;
    }
    Ok(ModelIndex::build_with_dependencies(files, dependency))
}
