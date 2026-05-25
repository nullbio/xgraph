//! Git worktree discovery and project identity.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::FromUtf8Error;

use thiserror::Error;

/// Canonical absolute path to a Git worktree root.
///
/// Construct with [`WorktreeRoot::discover`]. The internal path is opaque;
/// callers should prefer named operations over raw path access. The
/// [`WorktreeRoot::as_path`] accessor exists for cases where a `&Path` is
/// strictly required (such as feeding into another subprocess invocation).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorktreeRoot {
    path: PathBuf,
}

impl WorktreeRoot {
    /// Resolve the canonical Git worktree root containing `start`.
    ///
    /// Invokes `git -C <start> rev-parse --show-toplevel`. Returns the
    /// absolute canonical path on success. Returns
    /// [`GitDiscoveryError::NotInWorktree`] when `start` is not inside any
    /// Git worktree (including bare repositories, which have no worktree).
    /// No filesystem state is created on failure.
    pub fn discover(start: &Path) -> Result<Self, GitDiscoveryError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(start)
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .map_err(|source| GitDiscoveryError::GitInvocation { source })?;

        if !output.status.success() {
            return Err(GitDiscoveryError::NotInWorktree {
                start: start.to_path_buf(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let stdout = String::from_utf8(output.stdout).map_err(|source| {
            GitDiscoveryError::NonUtf8Output {
                stream: GitOutputStream::Stdout,
                source,
            }
        })?;
        let trimmed = stdout.trim_end_matches(['\n', '\r']);

        if trimmed.is_empty() {
            return Err(GitDiscoveryError::EmptyOutput);
        }

        Ok(Self {
            path: PathBuf::from(trimmed),
        })
    }

    /// Inspect the canonical absolute worktree path.
    ///
    /// Prefer named operations on this type when adding new behavior so that
    /// internal representation can change without breaking callers.
    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

/// Errors produced while discovering a Git worktree.
#[derive(Debug, Error)]
pub enum GitDiscoveryError {
    #[error("failed to invoke `git`: {source}")]
    GitInvocation {
        #[source]
        source: std::io::Error,
    },

    #[error("path {start:?} is not inside a Git worktree: {stderr}")]
    NotInWorktree { start: PathBuf, stderr: String },

    #[error("`git rev-parse --show-toplevel` returned empty output")]
    EmptyOutput,

    #[error("`git` produced non-UTF-8 output on {stream}: {source}")]
    NonUtf8Output {
        stream: GitOutputStream,
        #[source]
        source: FromUtf8Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOutputStream {
    Stdout,
    Stderr,
}

impl std::fmt::Display for GitOutputStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdout => f.write_str("stdout"),
            Self::Stderr => f.write_str("stderr"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Private mirror of `tests/support::TempGitRepo` so unit tests inside
    /// this module do not depend on the integration-test support crate.
    struct TempGitRepo {
        root: PathBuf,
    }

    impl TempGitRepo {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos();
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "xgraph-git-test-{}-{}-{}",
                std::process::id(),
                nanos,
                counter
            ));

            fs::create_dir_all(&root).expect("failed to create temporary repo directory");

            let status = Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(&root)
                .status()
                .expect("failed to run git init");
            assert!(status.success(), "git init failed with status {status}");

            Self { root }
        }

        fn root(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempGitRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn temp_non_git_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "xgraph-git-test-nongit-{}-{}-{}",
            std::process::id(),
            nanos,
            counter
        ));
        fs::create_dir_all(&dir).expect("failed to create temporary non-git directory");
        dir
    }

    #[test]
    fn discover_returns_canonical_absolute_path_for_repo_root() {
        let repo = TempGitRepo::new();

        let root = WorktreeRoot::discover(repo.root()).expect("discover should succeed in repo");

        assert!(root.as_path().is_absolute());
        let canonical_expected = fs::canonicalize(repo.root()).expect("canonicalize repo root");
        let canonical_actual =
            fs::canonicalize(root.as_path()).expect("canonicalize discovered root");
        assert_eq!(canonical_actual, canonical_expected);
    }

    #[test]
    fn discover_from_subdirectory_finds_repo_root() {
        let repo = TempGitRepo::new();
        let nested = repo.root().join("a").join("b").join("c");
        fs::create_dir_all(&nested).expect("create nested dir");

        let root = WorktreeRoot::discover(&nested).expect("discover should succeed from subdir");

        let canonical_expected = fs::canonicalize(repo.root()).expect("canonicalize repo root");
        let canonical_actual =
            fs::canonicalize(root.as_path()).expect("canonicalize discovered root");
        assert_eq!(canonical_actual, canonical_expected);
    }

    #[test]
    fn discover_outside_worktree_returns_typed_error_and_creates_no_state() {
        let dir = temp_non_git_dir();
        let entries_before: Vec<_> = fs::read_dir(&dir)
            .expect("read non-git dir before")
            .collect::<Result<_, _>>()
            .expect("collect entries before");

        let err = WorktreeRoot::discover(&dir).expect_err("discover should fail outside worktree");
        match &err {
            GitDiscoveryError::NotInWorktree { start, .. } => {
                assert_eq!(start, &dir);
            }
            other => panic!("expected NotInWorktree, got {other:?}"),
        }

        let entries_after: Vec<_> = fs::read_dir(&dir)
            .expect("read non-git dir after")
            .collect::<Result<_, _>>()
            .expect("collect entries after");
        assert_eq!(
            entries_before.len(),
            entries_after.len(),
            "discover must not create any files in non-git directory",
        );
        assert!(!dir.join(".git").exists(), "must not create .git");
        assert!(!dir.join("xgraph").exists(), "must not create xgraph");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_on_nonexistent_path_returns_not_in_worktree() {
        let nonexistent = std::env::temp_dir().join(format!(
            "xgraph-git-test-missing-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        assert!(!nonexistent.exists());

        let err =
            WorktreeRoot::discover(&nonexistent).expect_err("discover should fail on missing path");
        assert!(
            matches!(err, GitDiscoveryError::NotInWorktree { .. }),
            "expected NotInWorktree, got {err:?}"
        );
    }
}
