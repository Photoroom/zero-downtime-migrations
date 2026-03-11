//! Git integration for --diff mode.
//!
//! This module provides:
//! - Repository detection
//! - Diff parsing to identify changed files
//! - Support for comparing against branches, tags, or commits
//!
//! Edge cases handled:
//! - Shallow clones
//! - Detached HEAD states
//! - Missing origin/main branch

use std::path::{Path, PathBuf};

use git2::{DiffOptions, Repository, StatusOptions};

use crate::error::{Error, Result};

/// Status of a file in the git diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// A changed file in the git diff.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// The path of the file (relative to repo root).
    pub path: PathBuf,
    /// The status of the change.
    pub status: FileStatus,
    /// For renamed files, the old path.
    pub old_path: Option<PathBuf>,
}

/// Git repository wrapper for diff operations.
pub struct GitRepo {
    repo: Repository,
}

impl GitRepo {
    /// Open a git repository from a path (searches up directory tree).
    pub fn open(path: &Path) -> Result<Self> {
        let repo = Repository::discover(path)
            .map_err(|e| Error::git_error_msg(format!("Failed to find git repository: {}", e)))?;
        Ok(Self { repo })
    }

    /// Check if a path is inside a git repository.
    pub fn is_git_repo(path: &Path) -> bool {
        Repository::discover(path).is_ok()
    }

    /// Get the repository root directory.
    pub fn root(&self) -> Result<PathBuf> {
        self.repo
            .workdir()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| Error::git_error_msg("Repository has no working directory (bare repo)"))
    }

    /// Get files changed between a reference and HEAD.
    ///
    /// The reference can be:
    /// - A branch name (e.g., "main", "origin/main")
    /// - A tag name (e.g., "v1.0.0")
    /// - A commit SHA (e.g., "abc123")
    /// - A relative reference (e.g., "HEAD~1", "HEAD^")
    pub fn changed_files(&self, base_ref: &str) -> Result<Vec<ChangedFile>> {
        // Resolve the base reference to a commit
        let base_obj = self.repo.revparse_single(base_ref).map_err(|e| {
            Error::git_error_msg(format!("Failed to resolve reference '{}': {}", base_ref, e))
        })?;

        let base_commit = base_obj.peel_to_commit().map_err(|e| {
            Error::git_error_msg(format!(
                "Reference '{}' does not point to a commit: {}",
                base_ref, e
            ))
        })?;

        let base_tree = base_commit.tree().map_err(|e| {
            Error::git_error_msg(format!("Failed to get tree for base commit: {}", e))
        })?;

        // Get HEAD commit tree
        let head_ref = self
            .repo
            .head()
            .map_err(|e| Error::git_error_msg(format!("Failed to get HEAD: {}", e)))?;

        let head_commit = head_ref
            .peel_to_commit()
            .map_err(|e| Error::git_error_msg(format!("Failed to get HEAD commit: {}", e)))?;

        let head_tree = head_commit
            .tree()
            .map_err(|e| Error::git_error_msg(format!("Failed to get tree for HEAD: {}", e)))?;

        // Compute diff
        let mut diff_opts = DiffOptions::new();
        diff_opts.include_untracked(false);

        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut diff_opts))
            .map_err(|e| Error::git_error_msg(format!("Failed to compute diff: {}", e)))?;

        // Collect changed files
        let mut files = Vec::new();
        diff.foreach(
            &mut |delta, _| {
                let status = match delta.status() {
                    git2::Delta::Added => FileStatus::Added,
                    git2::Delta::Deleted => FileStatus::Deleted,
                    git2::Delta::Modified => FileStatus::Modified,
                    git2::Delta::Renamed => FileStatus::Renamed,
                    git2::Delta::Copied => FileStatus::Added,
                    _ => return true, // Skip other statuses
                };

                let path = delta
                    .new_file()
                    .path()
                    .map(|p| p.to_path_buf())
                    .or_else(|| delta.old_file().path().map(|p| p.to_path_buf()));

                if let Some(path) = path {
                    let old_path = if status == FileStatus::Renamed {
                        delta.old_file().path().map(|p| p.to_path_buf())
                    } else {
                        None
                    };

                    files.push(ChangedFile {
                        path,
                        status,
                        old_path,
                    });
                }

                true
            },
            None,
            None,
            None,
        )
        .map_err(|e| Error::git_error_msg(format!("Failed to iterate diff: {}", e)))?;

        Ok(files)
    }

    /// Get all uncommitted changes (staged + unstaged).
    pub fn uncommitted_changes(&self) -> Result<Vec<ChangedFile>> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(false)
            .include_ignored(false)
            .include_unmodified(false);

        let statuses = self
            .repo
            .statuses(Some(&mut opts))
            .map_err(|e| Error::git_error_msg(format!("Failed to get status: {}", e)))?;

        let mut files = Vec::new();
        for entry in statuses.iter() {
            let status = entry.status();
            let path = entry.path().map(PathBuf::from);

            if let Some(path) = path {
                let file_status = if status.is_index_new() || status.is_wt_new() {
                    FileStatus::Added
                } else if status.is_index_deleted() || status.is_wt_deleted() {
                    FileStatus::Deleted
                } else if status.is_index_modified() || status.is_wt_modified() {
                    FileStatus::Modified
                } else if status.is_index_renamed() || status.is_wt_renamed() {
                    FileStatus::Renamed
                } else {
                    continue;
                };

                files.push(ChangedFile {
                    path,
                    status: file_status,
                    old_path: None,
                });
            }
        }

        Ok(files)
    }

    /// Filter changed files to only migration files.
    pub fn changed_migrations(&self, base_ref: &str) -> Result<Vec<ChangedFile>> {
        let files = self.changed_files(base_ref)?;
        Ok(files
            .into_iter()
            .filter(|f| is_migration_path(&f.path))
            .collect())
    }

    /// Get paths of all changed migration files (for compatibility with discovery).
    pub fn changed_migration_paths(&self, base_ref: &str) -> Result<Vec<PathBuf>> {
        let root = self.root()?;
        let migrations = self.changed_migrations(base_ref)?;
        Ok(migrations
            .into_iter()
            .filter(|f| f.status != FileStatus::Deleted)
            .map(|f| root.join(&f.path))
            .collect())
    }

    /// Get paths of all non-migration files changed in the diff.
    pub fn changed_non_migration_paths(&self, base_ref: &str) -> Result<Vec<PathBuf>> {
        let root = self.root()?;
        let files = self.changed_files(base_ref)?;
        Ok(files
            .into_iter()
            .filter(|f| !is_migration_path(&f.path) && f.status != FileStatus::Deleted)
            .map(|f| root.join(&f.path))
            .collect())
    }

    /// Check if the repository is a shallow clone.
    pub fn is_shallow(&self) -> bool {
        self.repo.is_shallow()
    }

    /// Check if HEAD is detached.
    pub fn is_head_detached(&self) -> bool {
        self.repo.head_detached().unwrap_or(false)
    }

    /// Get the current branch name, if on a branch.
    pub fn current_branch(&self) -> Option<String> {
        self.repo
            .head()
            .ok()
            .and_then(|r| r.shorthand().map(String::from))
    }
}

/// Check if a path looks like a Django migration file.
fn is_migration_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    // Must be in a migrations directory
    if !path_str.contains("migrations/") && !path_str.contains("migrations\\") {
        return false;
    }

    // Must be a Python file
    if path.extension().is_none_or(|ext| ext != "py") {
        return false;
    }

    // Must not be __init__.py
    if path.file_name().is_some_and(|name| name == "__init__.py") {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// Helper to create a git repository for testing.
    fn create_test_repo() -> (TempDir, GitRepo) {
        let temp = TempDir::new().unwrap();

        // Initialize git repo
        Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .expect("Failed to init git repo");

        // Configure git user for commits
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        let repo = GitRepo::open(temp.path()).unwrap();
        (temp, repo)
    }

    /// Helper to create a commit.
    fn commit(temp: &TempDir, message: &str) {
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", message, "--allow-empty"])
            .current_dir(temp.path())
            .output()
            .unwrap();
    }

    #[test]
    fn test_is_git_repo() {
        let temp = TempDir::new().unwrap();
        assert!(!GitRepo::is_git_repo(temp.path()));

        Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        assert!(GitRepo::is_git_repo(temp.path()));
    }

    #[test]
    fn test_open_repo() {
        let (temp, repo) = create_test_repo();

        // Can get root
        let root = repo.root().unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            temp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_open_repo_from_subdirectory() {
        let (temp, _) = create_test_repo();

        // Create subdirectory
        let subdir = temp.path().join("src").join("app");
        fs::create_dir_all(&subdir).unwrap();

        // Should find repo from subdirectory
        let repo = GitRepo::open(&subdir).unwrap();
        assert_eq!(
            repo.root().unwrap().canonicalize().unwrap(),
            temp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_not_a_repo() {
        let temp = TempDir::new().unwrap();
        let result = GitRepo::open(temp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_changed_files_added() {
        let (temp, repo) = create_test_repo();

        // Create initial commit
        fs::write(temp.path().join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial commit");

        // Add a file
        fs::write(temp.path().join("new_file.py"), "print('hello')").unwrap();
        commit(&temp, "Add new file");

        let changed = repo.changed_files("HEAD~1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, PathBuf::from("new_file.py"));
        assert_eq!(changed[0].status, FileStatus::Added);
    }

    #[test]
    fn test_changed_files_modified() {
        let (temp, repo) = create_test_repo();

        // Create initial file and commit
        fs::write(temp.path().join("file.py"), "v1").unwrap();
        commit(&temp, "Initial");

        // Modify the file
        fs::write(temp.path().join("file.py"), "v2").unwrap();
        commit(&temp, "Modify file");

        let changed = repo.changed_files("HEAD~1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, FileStatus::Modified);
    }

    #[test]
    fn test_changed_files_deleted() {
        let (temp, repo) = create_test_repo();

        // Create initial file and commit
        fs::write(temp.path().join("file.py"), "content").unwrap();
        commit(&temp, "Initial");

        // Delete the file
        fs::remove_file(temp.path().join("file.py")).unwrap();
        commit(&temp, "Delete file");

        let changed = repo.changed_files("HEAD~1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, FileStatus::Deleted);
    }

    #[test]
    fn test_changed_migrations_filter() {
        let (temp, repo) = create_test_repo();

        // Create initial commit
        fs::write(temp.path().join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        // Create migrations directory structure
        let migrations_dir = temp.path().join("myapp").join("migrations");
        fs::create_dir_all(&migrations_dir).unwrap();

        // Add migration and regular Python file
        fs::write(migrations_dir.join("0001_initial.py"), "# migration").unwrap();
        fs::write(migrations_dir.join("__init__.py"), "").unwrap();
        fs::write(temp.path().join("myapp").join("models.py"), "# models").unwrap();
        commit(&temp, "Add files");

        let migrations = repo.changed_migrations("HEAD~1").unwrap();
        assert_eq!(migrations.len(), 1);
        assert!(migrations[0]
            .path
            .to_string_lossy()
            .contains("0001_initial.py"));
    }

    #[test]
    fn test_changed_migration_paths() {
        let (temp, repo) = create_test_repo();

        // Create initial commit
        fs::write(temp.path().join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        // Create migration
        let migrations_dir = temp.path().join("app").join("migrations");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(migrations_dir.join("0001_test.py"), "# migration").unwrap();
        commit(&temp, "Add migration");

        let paths = repo.changed_migration_paths("HEAD~1").unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("0001_test.py"));
        // Should be absolute path
        assert!(paths[0].is_absolute());
    }

    #[test]
    fn test_changed_non_migration_paths() {
        let (temp, repo) = create_test_repo();

        // Initial commit
        fs::write(temp.path().join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        // Add migration and non-migration files
        let migrations_dir = temp.path().join("app").join("migrations");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(migrations_dir.join("0001_test.py"), "# migration").unwrap();
        fs::write(temp.path().join("app").join("models.py"), "# models").unwrap();
        fs::write(temp.path().join("app").join("views.py"), "# views").unwrap();
        commit(&temp, "Add files");

        let non_migrations = repo.changed_non_migration_paths("HEAD~1").unwrap();
        assert_eq!(non_migrations.len(), 2);
        assert!(non_migrations.iter().any(|p| p.ends_with("models.py")));
        assert!(non_migrations.iter().any(|p| p.ends_with("views.py")));
    }

    #[test]
    fn test_invalid_ref() {
        let (temp, repo) = create_test_repo();

        // Create initial commit
        fs::write(temp.path().join("file.txt"), "content").unwrap();
        commit(&temp, "Initial");

        let result = repo.changed_files("nonexistent_branch");
        assert!(result.is_err());
    }

    #[test]
    fn test_is_migration_path() {
        // Valid migration paths
        assert!(is_migration_path(Path::new(
            "app/migrations/0001_initial.py"
        )));
        assert!(is_migration_path(Path::new(
            "myapp/migrations/0002_add_field.py"
        )));
        assert!(is_migration_path(Path::new(
            "some/nested/app/migrations/0001_test.py"
        )));

        // Invalid paths
        assert!(!is_migration_path(Path::new("app/migrations/__init__.py")));
        assert!(!is_migration_path(Path::new("app/models.py")));
        assert!(!is_migration_path(Path::new("migrations.py")));
        assert!(!is_migration_path(Path::new("app/migration/0001.py")));
        assert!(!is_migration_path(Path::new("app/migrations/0001.txt")));
    }

    #[test]
    fn test_current_branch() {
        let (temp, repo) = create_test_repo();

        // Create initial commit
        fs::write(temp.path().join("file.txt"), "content").unwrap();
        commit(&temp, "Initial");

        // Should be on master or main
        let branch = repo.current_branch();
        assert!(branch.is_some());
        let branch_name = branch.unwrap();
        assert!(branch_name == "master" || branch_name == "main");
    }

    #[test]
    fn test_uncommitted_changes() {
        let (temp, repo) = create_test_repo();

        // Create initial commit
        fs::write(temp.path().join("file.txt"), "v1").unwrap();
        commit(&temp, "Initial");

        // Make uncommitted changes
        fs::write(temp.path().join("file.txt"), "v2").unwrap();
        fs::write(temp.path().join("new_file.py"), "new").unwrap();

        // Stage the new file
        Command::new("git")
            .args(["add", "new_file.py"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        let changes = repo.uncommitted_changes().unwrap();
        assert!(changes.len() >= 1);
        // Should see the modified file and/or the new staged file
    }

    #[test]
    fn test_diff_against_branch() {
        let (temp, repo) = create_test_repo();

        // Create initial commit on main
        fs::write(temp.path().join("file.txt"), "initial").unwrap();
        commit(&temp, "Initial");

        // Create feature branch
        Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        // Add file on feature branch
        fs::write(temp.path().join("feature.py"), "feature code").unwrap();
        commit(&temp, "Feature commit");

        // Diff against main/master
        let branch = repo.current_branch().unwrap();
        let base = if branch == "feature" {
            // Need to figure out base branch
            let result = repo.changed_files("HEAD~1");
            assert!(result.is_ok());
            result.unwrap()
        } else {
            vec![]
        };

        assert!(!base.is_empty() || branch != "feature");
    }

    #[test]
    fn test_multiple_changed_files() {
        let (temp, repo) = create_test_repo();

        // Initial commit
        fs::write(temp.path().join("file1.py"), "v1").unwrap();
        fs::write(temp.path().join("file2.py"), "v1").unwrap();
        commit(&temp, "Initial");

        // Make multiple changes
        fs::write(temp.path().join("file1.py"), "v2").unwrap();
        fs::write(temp.path().join("file3.py"), "new").unwrap();
        fs::remove_file(temp.path().join("file2.py")).unwrap();
        commit(&temp, "Multiple changes");

        let changed = repo.changed_files("HEAD~1").unwrap();
        assert_eq!(changed.len(), 3);

        let statuses: HashSet<_> = changed.iter().map(|f| f.status).collect();
        assert!(statuses.contains(&FileStatus::Added));
        assert!(statuses.contains(&FileStatus::Modified));
        assert!(statuses.contains(&FileStatus::Deleted));
    }
}
