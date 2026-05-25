//! Cozo schema, migrations, and query layer.
//!
//! This module also owns persistent path resolution for xgraph state. All
//! paths are derived from `git rev-parse --git-path xgraph` so that state
//! lives inside the worktree's private Git directory, never in tracked
//! project files.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::FromUtf8Error;

use thiserror::Error;

use crate::git::WorktreeRoot;

const CONFIG_FILE_NAME: &str = "config.toml";
const COZO_DB_DIR_NAME: &str = "graph.cozo";
const SCHEMA_VERSION_FILE_NAME: &str = "schema.version";

/// Resolved persistent storage paths for a single worktree.
///
/// Constructed from a [`WorktreeRoot`] via [`PersistentPaths::for_worktree`].
/// Construction is side-effect free: no directories are created. Use
/// [`PersistentPaths::ensure_created`] explicitly when callers want the
/// directory layout materialized.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PersistentPaths {
    root: PathBuf,
}

impl PersistentPaths {
    /// Resolve `$(git -C <worktree> rev-parse --git-path xgraph)` against
    /// the worktree root and return the resulting paths.
    pub fn for_worktree(worktree: &WorktreeRoot) -> Result<Self, PersistentPathsError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(worktree.as_path())
            .arg("rev-parse")
            .arg("--git-path")
            .arg("xgraph")
            .output()
            .map_err(|source| PersistentPathsError::GitInvocation { source })?;

        if !output.status.success() {
            return Err(PersistentPathsError::GitFailed {
                worktree: worktree.as_path().to_path_buf(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let stdout = String::from_utf8(output.stdout)
            .map_err(|source| PersistentPathsError::NonUtf8Output { source })?;
        let trimmed = stdout.trim_end_matches(['\n', '\r']);

        if trimmed.is_empty() {
            return Err(PersistentPathsError::EmptyOutput);
        }

        // `git rev-parse --git-path xgraph` returns either a path relative
        // to the worktree root (main worktree case, e.g. `.git/xgraph`) or
        // an absolute path (linked worktree case, e.g.
        // `/repo/.git/worktrees/feat/xgraph`). `Path::join` handles both:
        // joining with an absolute path discards the base.
        let root = worktree.as_path().join(trimmed);

        Ok(Self { root })
    }

    /// Path to the persistent root directory containing all xgraph state
    /// for this worktree.
    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    /// Path to the per-worktree configuration file.
    pub fn config_toml_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILE_NAME)
    }

    /// Path to the embedded Cozo database directory.
    pub fn cozo_db_path(&self) -> PathBuf {
        self.root.join(COZO_DB_DIR_NAME)
    }

    /// Path to the schema version marker file.
    pub fn schema_version_path(&self) -> PathBuf {
        self.root.join(SCHEMA_VERSION_FILE_NAME)
    }

    /// Create the persistent root directory and the Cozo database
    /// directory. Idempotent. Files (`config.toml`, `schema.version`) are
    /// the caller's responsibility to write.
    pub fn ensure_created(&self) -> Result<(), PersistentPathsError> {
        for dir in [self.root.as_path(), self.cozo_db_path().as_path()] {
            std::fs::create_dir_all(dir).map_err(|source| {
                PersistentPathsError::EnsureCreateFailed {
                    path: dir.to_path_buf(),
                    source,
                }
            })?;
        }
        Ok(())
    }
}

/// Errors produced while resolving or materializing persistent paths.
#[derive(Debug, Error)]
pub enum PersistentPathsError {
    #[error("failed to invoke `git`: {source}")]
    GitInvocation {
        #[source]
        source: std::io::Error,
    },

    #[error("`git rev-parse --git-path xgraph` failed in worktree {worktree:?}: {stderr}")]
    GitFailed { worktree: PathBuf, stderr: String },

    #[error("`git rev-parse --git-path xgraph` returned empty output")]
    EmptyOutput,

    #[error("`git` produced non-UTF-8 output: {source}")]
    NonUtf8Output {
        #[source]
        source: FromUtf8Error,
    },

    #[error("failed to create directory {path:?}: {source}")]
    EnsureCreateFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
                "xgraph-storage-test-{}-{}-{}",
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

    #[test]
    fn for_worktree_derives_subpaths_under_git_path_xgraph() {
        let repo = TempGitRepo::new();
        let worktree =
            WorktreeRoot::discover(repo.root()).expect("worktree discovery should succeed");

        let paths = PersistentPaths::for_worktree(&worktree).expect("paths resolution");

        let canonical_repo = fs::canonicalize(repo.root()).expect("canonicalize repo root");
        let expected_root = canonical_repo.join(".git").join("xgraph");
        assert_eq!(paths.root_dir(), expected_root.as_path());

        assert_eq!(
            paths.config_toml_path(),
            expected_root.join("config.toml"),
            "config.toml path"
        );
        assert_eq!(
            paths.cozo_db_path(),
            expected_root.join("graph.cozo"),
            "cozo db dir"
        );
        assert_eq!(
            paths.schema_version_path(),
            expected_root.join("schema.version"),
            "schema version marker"
        );
    }

    #[test]
    fn for_worktree_does_not_create_any_directories() {
        let repo = TempGitRepo::new();
        let worktree =
            WorktreeRoot::discover(repo.root()).expect("worktree discovery should succeed");

        let paths = PersistentPaths::for_worktree(&worktree).expect("paths resolution");

        assert!(
            !paths.root_dir().exists(),
            "constructing PersistentPaths must not create the root dir"
        );
        assert!(!paths.cozo_db_path().exists());
        assert!(!paths.config_toml_path().exists());
        assert!(!paths.schema_version_path().exists());
    }

    #[test]
    fn ensure_created_materializes_directory_layout() {
        let repo = TempGitRepo::new();
        let worktree =
            WorktreeRoot::discover(repo.root()).expect("worktree discovery should succeed");
        let paths = PersistentPaths::for_worktree(&worktree).expect("paths resolution");

        paths
            .ensure_created()
            .expect("ensure_created should succeed");

        assert!(paths.root_dir().is_dir(), "root dir should exist");
        assert!(paths.cozo_db_path().is_dir(), "cozo db dir should exist");
        assert!(
            !paths.config_toml_path().exists(),
            "config file must not be created by ensure_created"
        );
        assert!(
            !paths.schema_version_path().exists(),
            "schema version file must not be created by ensure_created"
        );
    }

    #[test]
    fn ensure_created_is_idempotent() {
        let repo = TempGitRepo::new();
        let worktree =
            WorktreeRoot::discover(repo.root()).expect("worktree discovery should succeed");
        let paths = PersistentPaths::for_worktree(&worktree).expect("paths resolution");

        paths.ensure_created().expect("first ensure_created");
        paths
            .ensure_created()
            .expect("second ensure_created should also succeed");

        assert!(paths.root_dir().is_dir());
        assert!(paths.cozo_db_path().is_dir());
    }
}
