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

use git2::{DiffOptions, Repository};

use crate::discovery::is_migration_file;
use crate::error::{Error, Result};

/// Status of a file in the git diff.
///
/// `#[non_exhaustive]`: libgit2 distinguishes Copied,
/// Typechange, Unmodified, etc.; we surface only the four we
/// currently act on, and reserve room to surface more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// Which subset of changed files a caller wants.
#[derive(Debug, Clone, Copy)]
enum ChangedKind {
    Migrations,
    NonMigrations,
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
        let base_tree = self.tree_at(base_ref)?;
        let head_tree = self.head_tree()?;
        let mut diff_opts = DiffOptions::new();
        diff_opts.include_untracked(false);
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut diff_opts))
            .map_err(|e| Error::git_error_msg(format!("Failed to compute diff: {}", e)))?;
        collect_changed_files(&diff)
    }

    /// Get staged files changed between a reference and the git index.
    ///
    /// This is intended for pre-commit hooks, where HEAD still points at the
    /// previous commit and the commit being checked exists only in the index.
    pub fn changed_staged_files(&self, base_ref: &str) -> Result<Vec<ChangedFile>> {
        let base_tree = self.tree_at(base_ref)?;
        let index = self
            .repo
            .index()
            .map_err(|e| Error::git_error_msg(format!("Failed to read git index: {}", e)))?;
        let mut diff_opts = DiffOptions::new();
        diff_opts.include_untracked(false);
        let diff = self
            .repo
            .diff_tree_to_index(Some(&base_tree), Some(&index), Some(&mut diff_opts))
            .map_err(|e| Error::git_error_msg(format!("Failed to compute staged diff: {}", e)))?;
        collect_changed_files(&diff)
    }

    /// Resolve a reference to the tree at that commit. If the ref does
    /// not exist (the common `--diff origin/main` case after a shallow
    /// clone or fork checkout), returns `Error::InvalidGitReference` so
    /// the CLI can surface a targeted message; other libgit2 failures
    /// fall through as generic `GitError`s.
    fn tree_at(&self, base_ref: &str) -> Result<git2::Tree<'_>> {
        let base_obj = self.repo.revparse_single(base_ref).map_err(|e| {
            // libgit2's NotFound also fires for things like `HEAD~999`
            // (commit out of range) or revspecs whose intermediate
            // objects are missing. Surfacing all of those as "invalid
            // git reference" is still the right user-facing framing —
            // the user supplied something that didn't resolve.
            if e.code() == git2::ErrorCode::NotFound {
                Error::InvalidGitReference {
                    reference: base_ref.to_string(),
                }
            } else {
                Error::git_error_msg(format!("Failed to resolve reference '{}': {}", base_ref, e))
            }
        })?;
        let base_commit = base_obj.peel_to_commit().map_err(|e| {
            Error::git_error_msg(format!(
                "Reference '{}' does not point to a commit: {}",
                base_ref, e
            ))
        })?;
        base_commit
            .tree()
            .map_err(|e| Error::git_error_msg(format!("Failed to get tree for base commit: {}", e)))
    }

    /// Get the tree at HEAD.
    fn head_tree(&self) -> Result<git2::Tree<'_>> {
        let head_ref = self
            .repo
            .head()
            .map_err(|e| Error::git_error_msg(format!("Failed to get HEAD: {}", e)))?;
        let head_commit = head_ref
            .peel_to_commit()
            .map_err(|e| Error::git_error_msg(format!("Failed to get HEAD commit: {}", e)))?;
        head_commit
            .tree()
            .map_err(|e| Error::git_error_msg(format!("Failed to get tree for HEAD: {}", e)))
    }

    /// Read the staged contents of a file from the git index.
    ///
    /// `path` may be absolute (inside the repo root) or already
    /// repo-relative. The function does not touch the filesystem to
    /// resolve `path` — earlier code called `path.canonicalize()` to cope
    /// with macOS-style `/tmp` ↔ `/private/tmp` aliases, but that
    /// required `path` to exist on disk, which is wrong for a function
    /// whose purpose is to read content that may exist only in the
    /// index. The size cap from `parser::check_size` is applied against
    /// the staged blob's object size before its content bytes are read.
    pub fn read_staged_file(&self, path: &Path) -> Result<String> {
        let root = self.root()?;
        let relative_path: PathBuf = if path.is_absolute() {
            path.strip_prefix(&root)
                .map_err(|_| {
                    Error::git_error_msg(format!(
                        "File '{}' is not inside repository '{}'",
                        path.display(),
                        root.display()
                    ))
                })?
                .to_path_buf()
        } else {
            path.to_path_buf()
        };

        let index = self
            .repo
            .index()
            .map_err(|e| Error::git_error_msg(format!("Failed to read git index: {}", e)))?;
        let entry = index.get_path(&relative_path, 0).ok_or_else(|| {
            Error::git_error_msg(format!(
                "File '{}' is not present in the git index",
                relative_path.display()
            ))
        })?;
        let blob = self
            .repo
            .find_blob(entry.id)
            .map_err(|e| Error::git_error_msg(format!("Failed to read staged file: {}", e)))?;
        crate::parser::check_size(path, blob.size() as u64)?;

        std::str::from_utf8(blob.content())
            .map(|s| s.to_string())
            .map_err(|e| {
                Error::git_error_msg(format!(
                    "Staged file '{}' is not valid UTF-8: {}",
                    relative_path.display(),
                    e
                ))
            })
    }

    /// Get paths of all changed migration files (absolute paths under
    /// the repo root).
    pub fn changed_migration_paths(&self, base_ref: &str) -> Result<Vec<PathBuf>> {
        self.paths_from(self.changed_files(base_ref)?, ChangedKind::Migrations)
    }

    /// Get paths of all staged changed migration files.
    pub fn changed_staged_migration_paths(&self, base_ref: &str) -> Result<Vec<PathBuf>> {
        self.paths_from(
            self.changed_staged_files(base_ref)?,
            ChangedKind::Migrations,
        )
    }

    /// Get paths of all non-migration files changed in the diff.
    pub fn changed_non_migration_paths(&self, base_ref: &str) -> Result<Vec<PathBuf>> {
        self.paths_from(self.changed_files(base_ref)?, ChangedKind::NonMigrations)
    }

    /// Get paths of all staged non-migration files changed in the diff.
    pub fn changed_staged_non_migration_paths(&self, base_ref: &str) -> Result<Vec<PathBuf>> {
        self.paths_from(
            self.changed_staged_files(base_ref)?,
            ChangedKind::NonMigrations,
        )
    }

    /// Project a list of `ChangedFile`s into absolute paths, filtering
    /// out deletions and selecting either migration or non-migration files
    /// according to `kind`.
    fn paths_from(&self, files: Vec<ChangedFile>, kind: ChangedKind) -> Result<Vec<PathBuf>> {
        let root = self.root()?;
        let want_migrations = matches!(kind, ChangedKind::Migrations);
        Ok(files
            .into_iter()
            .filter(|f| f.status != FileStatus::Deleted)
            .filter(|f| is_migration_file(&f.path) == want_migrations)
            .map(|f| root.join(&f.path))
            .collect())
    }
}

/// Iterate a `git2::Diff` and project each `DiffDelta` into a
/// `ChangedFile`. Used by both `changed_files` (tree-to-tree) and
/// `changed_staged_files` (tree-to-index); the two paths only differ in
/// how they produced the diff.
fn collect_changed_files(diff: &git2::Diff<'_>) -> Result<Vec<ChangedFile>> {
    let mut files = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            let status = match delta.status() {
                git2::Delta::Added => FileStatus::Added,
                git2::Delta::Deleted => FileStatus::Deleted,
                git2::Delta::Modified => FileStatus::Modified,
                git2::Delta::Renamed => FileStatus::Renamed,
                git2::Delta::Copied => FileStatus::Added,
                _ => return true,
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
        // `is_err()` alone is too weak: an unrelated future error
        // (e.g. an InvalidPath, an IO failure) would still pass. Pin
        // the variant so a regression that swaps the error type
        // — say, returning ParseError because some refactor wired
        // GitRepo::open through a different boundary — fails loudly.
        let temp = TempDir::new().unwrap();
        // `unwrap_err()` requires `Ok` to be Debug, which `GitRepo`
        // isn't; pattern-match instead.
        let Err(err) = GitRepo::open(temp.path()) else {
            panic!("expected GitRepo::open to fail outside a repo")
        };
        assert!(
            matches!(err, crate::error::Error::GitError { .. }),
            "expected Error::GitError, got: {err:?}",
        );
        assert!(
            err.to_string().contains("Failed to find git repository"),
            "the error message should explain *what* failed, got: {err}",
        );
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

        fs::write(temp.path().join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        let migrations_dir = temp.path().join("myapp").join("migrations");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(migrations_dir.join("0001_initial.py"), "# migration").unwrap();
        fs::write(migrations_dir.join("__init__.py"), "").unwrap();
        fs::write(temp.path().join("myapp").join("models.py"), "# models").unwrap();
        commit(&temp, "Add files");

        // The filter keeps the migration and drops __init__.py and the
        // regular models.py.
        let migration_paths = repo.changed_migration_paths("HEAD~1").unwrap();
        assert_eq!(migration_paths.len(), 1);
        assert!(migration_paths[0]
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
    fn test_invalid_ref_returns_targeted_error() {
        let (temp, repo) = create_test_repo();

        fs::write(temp.path().join("file.txt"), "content").unwrap();
        commit(&temp, "Initial");

        let result = repo.changed_files("nonexistent_branch");
        match result {
            Err(Error::InvalidGitReference { reference }) => {
                assert_eq!(reference, "nonexistent_branch");
            }
            other => panic!("expected InvalidGitReference, got {other:?}"),
        }
    }

    // Migration-path matching now delegates to
    // `crate::discovery::is_migration_file`; see `test_is_migration_file`
    // in `src/discovery.rs` for the full case coverage.

    #[test]
    fn test_changed_staged_files_uses_index_not_head() {
        let (temp, repo) = create_test_repo();

        fs::write(temp.path().join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        Command::new("git")
            .args(["branch", "origin/main"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        let migrations_dir = temp.path().join("app").join("migrations");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(migrations_dir.join("__init__.py"), "").unwrap();
        fs::write(migrations_dir.join("0001_initial.py"), "# staged migration").unwrap();

        Command::new("git")
            .args(["add", "app/migrations/0001_initial.py"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        let head_changed = repo.changed_files("origin/main").unwrap();
        assert!(head_changed.is_empty());

        let staged_changed = repo.changed_staged_files("origin/main").unwrap();
        assert_eq!(staged_changed.len(), 1);
        assert_eq!(
            staged_changed[0].path,
            PathBuf::from("app/migrations/0001_initial.py")
        );
    }

    #[test]
    fn test_read_staged_file_ignores_unstaged_worktree_changes() {
        let (temp, repo) = create_test_repo();
        // Use `repo.root()` consistently so the prefix matches libgit2's
        // view; on macOS `temp.path()` and `repo.root()` can differ in
        // /tmp vs /private/tmp.
        let root = repo.root().unwrap();

        fs::write(root.join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        fs::write(root.join("file.py"), "staged").unwrap();
        Command::new("git")
            .args(["add", "file.py"])
            .current_dir(&root)
            .output()
            .unwrap();

        fs::write(root.join("file.py"), "unstaged").unwrap();

        let content = repo.read_staged_file(&root.join("file.py")).unwrap();
        assert_eq!(content, "staged");
    }

    // Note: a portable regression test for the old `canonicalize` path
    // would require manufacturing a repo whose libgit2 workdir prefix
    // differs from the path the caller hands in (the macOS
    // `/tmp` ↔ `/private/tmp` case) AND the worktree copy being absent.
    // That's hard to construct cross-platform; the relative-path test
    // below covers the only branch that survives in the new code, and
    // the absolute path under `repo.root()` is covered by
    // `test_read_staged_file_ignores_unstaged_worktree_changes` above.

    #[test]
    fn test_read_staged_file_accepts_repo_relative_path() {
        // Relative paths skip `strip_prefix` entirely; this is the path
        // a caller would take if they had the repo-relative form already
        // and never reconstructed an absolute path. It exercises the
        // is_absolute=false branch independently of any disk presence.
        let (temp, repo) = create_test_repo();
        let root = repo.root().unwrap();

        fs::write(root.join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        fs::write(root.join("file.py"), "v1").unwrap();
        Command::new("git")
            .args(["add", "file.py"])
            .current_dir(&root)
            .output()
            .unwrap();

        let content = repo.read_staged_file(Path::new("file.py")).unwrap();
        assert_eq!(content, "v1");
    }

    #[test]
    fn test_read_staged_file_rejects_oversized_blob() {
        // The size check must run *before* `find_blob`, so a multi-GB
        // staged blob can't force libgit2 to inflate it just to reject
        // it. We can't easily stage a GB-sized file in CI, but
        // `MAX_FILE_SIZE` is 10 MB, so a 10 MB + 1 byte file is enough
        // to drive the new pre-allocation guard.
        let (temp, repo) = create_test_repo();
        let root = repo.root().unwrap();

        fs::write(root.join("README.md"), "# Test").unwrap();
        commit(&temp, "Initial");

        let payload = vec![b'a'; (crate::parser::MAX_FILE_SIZE as usize) + 1];
        fs::write(root.join("huge.py"), &payload).unwrap();
        Command::new("git")
            .args(["add", "huge.py"])
            .current_dir(&root)
            .output()
            .unwrap();

        match repo.read_staged_file(&root.join("huge.py")) {
            Err(Error::FileTooLarge { size, max_size, .. }) => {
                assert_eq!(size, crate::parser::MAX_FILE_SIZE + 1);
                assert_eq!(max_size, crate::parser::MAX_FILE_SIZE);
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn test_diff_against_named_branch() {
        // Diffing against a named branch (rather than HEAD~N) exercises
        // the revparse_single → tree_at path for a real branch reference.
        let (temp, repo) = create_test_repo();
        let root = repo.root().unwrap();

        fs::write(root.join("file.txt"), "initial").unwrap();
        commit(&temp, "Initial");

        Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(&root)
            .output()
            .unwrap();
        fs::write(root.join("feature.py"), "feature code").unwrap();
        commit(&temp, "Feature commit");

        // master/main exists from the initial commit; diff against it.
        let base = ["master", "main"]
            .iter()
            .find_map(|name| repo.changed_files(name).ok())
            .expect("expected master or main to exist as the initial branch");
        assert_eq!(base.len(), 1);
        assert_eq!(base[0].path, PathBuf::from("feature.py"));
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
