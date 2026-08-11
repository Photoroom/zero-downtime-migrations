//! File discovery for Django and Alembic migration files.
//!
//! This module handles finding migration files in directories,
//! following the pattern `**/migrations/*.py`.

use std::path::{Path, PathBuf};

use glob::Pattern;
use walkdir::WalkDir;

use crate::error::{Error, Result};

/// The migration framework implied by a discovered path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationFramework {
    Django,
    Alembic,
}

/// Discovers Django migration files in the given paths, excluding those matching patterns.
///
/// Exclude patterns use glob syntax (e.g., "**/test_migrations/**", "**/fixtures/**").
pub fn discover_migrations_with_exclude(
    paths: &[PathBuf],
    exclude_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    let mut migrations = Vec::new();

    // Compile exclude patterns here rather than assuming they came
    // from `Config`: this is a public function and callers may pass
    // patterns directly.
    let patterns: Vec<Pattern> = exclude_patterns
        .iter()
        .map(|p| {
            Pattern::new(p).map_err(|e| Error::InvalidGlobPattern {
                pattern: p.clone(),
                message: e.to_string(),
            })
        })
        .collect::<Result<_>>()?;

    // Resolve once and reuse: `is_excluded` consults it for every file against
    // every pattern, so re-querying the OS per call would be wasteful.
    let current_dir = std::env::current_dir().ok();

    for path in paths {
        // `path.is_file()` transparently follows symlinks, so an
        // explicitly-passed symlink to `/etc/passwd` or `/dev/zero`
        // would otherwise reach the parser. The discovery WALK
        // already rejects symlinked entries via `symlink_metadata`
        // (see `discover_in_directory` below); apply the same
        // policy here so the explicit-path branch matches.
        if is_regular_file(path) {
            if is_migration_file(path) && !is_excluded(path, &patterns, current_dir.as_deref()) {
                migrations.push(path.clone());
            }
        } else if is_directory(path) {
            discover_in_directory(path, &mut migrations, &patterns, current_dir.as_deref())?;
        } else {
            return Err(Error::InvalidPath { path: path.clone() });
        }
    }

    // Sort for deterministic output
    migrations.sort();
    // Remove duplicates (in case same file is specified multiple ways)
    migrations.dedup();

    Ok(migrations)
}

/// `true` iff `path` exists, is a regular file, and is not a
/// symlink. `path.is_file()` from std follows links, which the
/// CLI's symlink-rejection policy specifically forbids — use
/// this helper everywhere a user-supplied path is being decided
/// between "file" and "directory".
fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file())
        .unwrap_or(false)
}

fn is_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
}

/// Check if a path matches any of the exclude patterns. `current_dir` is the
/// process working directory (resolved once by the caller) used to also test a
/// repo-relative form of `path`.
fn is_excluded(path: &Path, patterns: &[Pattern], current_dir: Option<&Path>) -> bool {
    let roots: Vec<&Path> = current_dir.into_iter().collect();
    path_matches_any_glob(path, &roots, patterns)
}

/// Match a path against its direct, `./`-stripped, and root-relative forms.
pub fn path_matches_any_glob(path: &Path, roots: &[&Path], patterns: &[Pattern]) -> bool {
    let mut candidates = vec![slash_path(path)];
    if let Ok(stripped) = path.strip_prefix(".") {
        candidates.push(slash_path(stripped));
    }
    candidates.extend(
        roots
            .iter()
            .filter_map(|root| path.strip_prefix(root).ok())
            .map(slash_path),
    );
    candidates
        .iter()
        .any(|candidate| patterns.iter().any(|p| p.matches(candidate)))
}

/// Render a path with `/` separators regardless of platform, for glob matching.
pub fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Recursively discover migration files in a directory.
fn discover_in_directory(
    dir: &Path,
    migrations: &mut Vec<PathBuf>,
    exclude_patterns: &[Pattern],
    current_dir: Option<&Path>,
) -> Result<()> {
    // `follow_links(false)` stops WalkDir from traversing symlinks
    // during the walk, but each yielded entry's `path` could itself
    // be a symlink. `path.is_file()` then follows the link, so a
    // hostile `0001.py -> /etc/passwd` symlink dropped inside a
    // migrations directory would otherwise get read. `entry.file_type()`
    // comes from a `symlink_metadata` call and reports the symlink as
    // a symlink, not its target — so a real file check rejects it.
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.map_err(|e| Error::directory_walk(dir, e))?;

        let path = entry.path();

        if entry.file_type().is_file()
            && is_migration_file(path)
            && !is_excluded(path, exclude_patterns, current_dir)
        {
            migrations.push(path.to_path_buf());
        }
    }

    Ok(())
}

/// Check if a path is a Django migration file.
///
/// A file is considered a migration if:
/// 1. It has a `.py` extension
/// 2. It's in a directory named `migrations`
/// 3. It's not `__init__.py`
pub fn is_migration_file(path: &Path) -> bool {
    // Must have .py extension
    if path.extension().is_none_or(|ext| ext != "py") {
        return false;
    }

    // Must not be __init__.py
    if let Some(filename) = path.file_name() {
        if filename == "__init__.py" {
            return false;
        }
    }

    // Must use one of the supported migration layouts.
    migration_framework(path).is_some()
}

/// Identify the supported migration layout for a path.
pub fn migration_framework(path: &Path) -> Option<MigrationFramework> {
    if path.extension().is_none_or(|ext| ext != "py") || path.file_name()? == "__init__.py" {
        return None;
    }
    if is_in_migrations_directory(path) {
        Some(MigrationFramework::Django)
    } else if is_in_alembic_versions_directory(path) {
        Some(MigrationFramework::Alembic)
    } else {
        None
    }
}

/// Check if a path is inside a `migrations` directory.
fn is_in_migrations_directory(path: &Path) -> bool {
    path.parent()
        .and_then(|p| p.file_name())
        .is_some_and(|name| name == "migrations")
}

/// Check whether a path is directly inside `alembic/versions`.
fn is_in_alembic_versions_directory(path: &Path) -> bool {
    path.parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "versions"))
        .and_then(Path::parent)
        .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "alembic"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a test directory structure with migrations.
    fn create_test_structure(temp: &TempDir) -> PathBuf {
        let root = temp.path();

        // Create app1/migrations/
        let app1_migrations = root.join("app1/migrations");
        fs::create_dir_all(&app1_migrations).unwrap();
        fs::write(app1_migrations.join("__init__.py"), "").unwrap();
        fs::write(app1_migrations.join("0001_initial.py"), "# migration 1").unwrap();
        fs::write(app1_migrations.join("0002_add_field.py"), "# migration 2").unwrap();

        // Create app2/migrations/
        let app2_migrations = root.join("app2/migrations");
        fs::create_dir_all(&app2_migrations).unwrap();
        fs::write(app2_migrations.join("__init__.py"), "").unwrap();
        fs::write(app2_migrations.join("0001_initial.py"), "# migration").unwrap();

        // Create a non-migration Python file
        fs::create_dir_all(root.join("app1")).unwrap();
        fs::write(root.join("app1/models.py"), "# models").unwrap();

        // Create a nested app
        let nested = root.join("apps/nested/migrations");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("0001_initial.py"), "# nested").unwrap();

        root.to_path_buf()
    }

    #[test]
    fn test_is_migration_file() {
        // Valid migration files
        assert!(is_migration_file(Path::new(
            "app/migrations/0001_initial.py"
        )));
        assert!(is_migration_file(Path::new(
            "app/migrations/0002_add_field.py"
        )));
        assert!(is_migration_file(Path::new(
            "/abs/path/app/migrations/0001_initial.py"
        )));
        assert!(is_migration_file(Path::new(
            "some/nested/app/migrations/0001_test.py"
        )));
        assert!(is_migration_file(Path::new(
            "service/alembic/versions/20260809_add_jobs.py"
        )));

        // Invalid: __init__.py
        assert!(!is_migration_file(Path::new("app/migrations/__init__.py")));

        // Invalid: not in migrations directory
        assert!(!is_migration_file(Path::new("app/models.py")));
        assert!(!is_migration_file(Path::new("app/0001_initial.py")));
        // A file literally named `migrations.py` at the repo root is not a
        // Django migration — the parent must be a `migrations/` directory.
        assert!(!is_migration_file(Path::new("migrations.py")));
        // Singular `migration/` directory does not count.
        assert!(!is_migration_file(Path::new("app/migration/0001.py")));
        assert!(!is_migration_file(Path::new("alembic/0001.py")));
        assert!(!is_migration_file(Path::new(
            "alembic/versions/__init__.py"
        )));

        // Invalid: not a .py file
        assert!(!is_migration_file(Path::new(
            "app/migrations/0001_initial.txt"
        )));
        assert!(!is_migration_file(Path::new("app/migrations/README.md")));
    }

    #[test]
    fn test_discover_migrations_in_directory() {
        let temp = TempDir::new().unwrap();
        let root = create_test_structure(&temp);

        let migrations =
            discover_migrations_with_exclude(std::slice::from_ref(&root), &[]).unwrap();

        // Should find 4 migrations (excluding __init__.py files)
        assert_eq!(migrations.len(), 4);

        // Should be sorted
        let filenames: Vec<_> = migrations
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        assert!(filenames.contains(&"0001_initial.py"));
        assert!(filenames.contains(&"0002_add_field.py"));
    }

    #[test]
    fn test_discover_single_file() {
        let temp = TempDir::new().unwrap();
        let root = create_test_structure(&temp);

        let file_path = root.join("app1/migrations/0001_initial.py");
        let migrations =
            discover_migrations_with_exclude(std::slice::from_ref(&file_path), &[]).unwrap();

        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0], file_path);
    }

    #[test]
    fn test_discover_multiple_paths() {
        let temp = TempDir::new().unwrap();
        let root = create_test_structure(&temp);

        let paths = vec![root.join("app1/migrations"), root.join("app2/migrations")];

        let migrations = discover_migrations_with_exclude(&paths, &[]).unwrap();

        assert_eq!(migrations.len(), 3); // 2 from app1 + 1 from app2
    }

    #[test]
    fn test_discover_deduplicates() {
        let temp = TempDir::new().unwrap();
        let root = create_test_structure(&temp);

        // Pass same directory twice
        let paths = vec![root.clone(), root.clone()];
        let migrations = discover_migrations_with_exclude(&paths, &[]).unwrap();

        // Should still only have 4 unique migrations
        assert_eq!(migrations.len(), 4);
    }

    #[test]
    fn test_discover_invalid_path_error() {
        let result =
            discover_migrations_with_exclude(&[PathBuf::from("/nonexistent/path/12345")], &[]);
        assert!(result.is_err());

        match result.unwrap_err() {
            Error::InvalidPath { path } => {
                assert_eq!(path, PathBuf::from("/nonexistent/path/12345"));
            }
            other => panic!("Expected InvalidPath error, got {:?}", other),
        }
    }

    #[test]
    fn test_discover_empty_directory() {
        let temp = TempDir::new().unwrap();
        let migrations =
            discover_migrations_with_exclude(&[temp.path().to_path_buf()], &[]).unwrap();
        assert!(migrations.is_empty());
    }

    #[test]
    fn test_discover_with_exclude_patterns() {
        let temp = TempDir::new().unwrap();
        let root = create_test_structure(&temp);

        // Exclude app1 migrations
        let exclude = vec!["**/app1/**".to_string()];
        let migrations =
            discover_migrations_with_exclude(std::slice::from_ref(&root), &exclude).unwrap();

        // Should only have migrations from app2 and nested (2 total)
        assert_eq!(migrations.len(), 2);

        // None should be from app1
        for m in &migrations {
            assert!(!m.to_string_lossy().contains("app1"));
        }
    }

    #[test]
    fn test_discover_invalid_exclude_pattern_errors() {
        let temp = TempDir::new().unwrap();
        let err =
            discover_migrations_with_exclude(&[temp.path().to_path_buf()], &["[".to_string()])
                .unwrap_err();

        match err {
            Error::InvalidGlobPattern { pattern, .. } => assert_eq!(pattern, "["),
            other => panic!("expected InvalidGlobPattern, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_skips_symlinked_files() {
        // A symlink dropped inside a migrations directory must not be
        // followed during discovery, even when it ends in `.py` — a
        // hostile `0001.py -> /etc/passwd` would otherwise have its
        // target ingested. The protection comes from
        // `entry.file_type().is_file()` (via `symlink_metadata`)
        // returning false for symlinks, rather than `path.is_file()`
        // which transparently follows them.
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a real migration file as the symlink target.
        let target_dir = root.join("real");
        fs::create_dir_all(&target_dir).unwrap();
        let target_file = target_dir.join("hidden.py");
        fs::write(&target_file, "# target").unwrap();

        // Create a migrations dir containing only a symlink pointing
        // at the file above.
        let migrations_dir = root.join("app/migrations");
        fs::create_dir_all(&migrations_dir).unwrap();
        let link_path = migrations_dir.join("0001_symlinked.py");
        symlink(&target_file, &link_path).unwrap();

        let migrations = discover_migrations_with_exclude(&[root.to_path_buf()], &[]).unwrap();

        assert!(
            migrations.is_empty(),
            "symlinked migration entry should have been skipped, got: {migrations:?}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_skips_symlinked_files_inside_excluded_path() {
        // Round-trip the symlink rejection with an exclude pattern
        // also in play: even if a future refactor reordered the
        // file_type check and the exclude check, the symlink should
        // never be ingested. Without this pin, swapping the two
        // checks would only break the unguarded variant above.
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let target_dir = root.join("real");
        fs::create_dir_all(&target_dir).unwrap();
        let target_file = target_dir.join("hidden.py");
        fs::write(&target_file, "# target").unwrap();

        // Symlink lives in a `test_migrations/migrations` dir that
        // the exclude pattern catches; the symlink-rejection must
        // hold regardless of exclude interaction.
        let migrations_dir = root.join("test_migrations/migrations");
        fs::create_dir_all(&migrations_dir).unwrap();
        let link_path = migrations_dir.join("0001_symlinked.py");
        symlink(&target_file, &link_path).unwrap();

        let exclude = vec!["**/test_migrations/**".to_string()];
        let migrations = discover_migrations_with_exclude(&[root.to_path_buf()], &exclude).unwrap();

        assert!(
            migrations.is_empty(),
            "symlink should be rejected even when an exclude pattern also matches, got: {migrations:?}",
        );
    }

    #[test]
    fn test_discover_with_specific_file_exclude() {
        let temp = TempDir::new().unwrap();
        let root = create_test_structure(&temp);

        // Exclude only 0001_initial.py files
        let exclude = vec!["**/0001_initial.py".to_string()];
        let migrations =
            discover_migrations_with_exclude(std::slice::from_ref(&root), &exclude).unwrap();

        // Should only have 0002_add_field.py from app1
        assert_eq!(migrations.len(), 1);
        assert!(migrations[0]
            .to_string_lossy()
            .contains("0002_add_field.py"));
    }
}
