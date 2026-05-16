//! File discovery for Django migration files.
//!
//! This module handles finding migration files in directories,
//! following the pattern `**/migrations/*.py`.

use std::path::{Path, PathBuf};

use glob::Pattern;
use walkdir::WalkDir;

use crate::error::{Error, Result};

/// Discovers Django migration files in the given paths, excluding those matching patterns.
///
/// Exclude patterns use glob syntax (e.g., "**/test_migrations/**", "**/fixtures/**").
pub fn discover_migrations_with_exclude(
    paths: &[PathBuf],
    exclude_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    let mut migrations = Vec::new();

    // Compile exclude patterns
    let patterns: Vec<Pattern> = exclude_patterns
        .iter()
        .filter_map(|p| Pattern::new(p).ok())
        .collect();

    for path in paths {
        if path.is_file() {
            if is_migration_file(path) && !is_excluded(path, &patterns) {
                migrations.push(path.clone());
            }
        } else if path.is_dir() {
            discover_in_directory(path, &mut migrations, &patterns)?;
        } else if !path.exists() {
            return Err(Error::InvalidPath { path: path.clone() });
        }
    }

    // Sort for deterministic output
    migrations.sort();
    // Remove duplicates (in case same file is specified multiple ways)
    migrations.dedup();

    Ok(migrations)
}

/// Check if a path matches any of the exclude patterns.
fn is_excluded(path: &Path, patterns: &[Pattern]) -> bool {
    let path_str = path.to_string_lossy();
    patterns.iter().any(|p| p.matches(&path_str))
}

/// Recursively discover migration files in a directory.
fn discover_in_directory(
    dir: &Path,
    migrations: &mut Vec<PathBuf>,
    exclude_patterns: &[Pattern],
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
            && !is_excluded(path, exclude_patterns)
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

    // Must be in a migrations directory
    is_in_migrations_directory(path)
}

/// Check if a path is inside a `migrations` directory.
fn is_in_migrations_directory(path: &Path) -> bool {
    path.parent()
        .and_then(|p| p.file_name())
        .is_some_and(|name| name == "migrations")
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
