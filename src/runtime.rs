//! Runtime paths, sockets, and OS-level locks.

use std::{
    fmt,
    fs::{DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use sha2::{Digest, Sha256};

/// Linux `sockaddr_un::sun_path` capacity, including the trailing NUL byte.
const SUN_PATH_CAPACITY: usize = 108;

/// Owner-only directory permissions (`rwx------`).
const RUNTIME_DIR_MODE: u32 = 0o700;

/// Owner-only lock-file permissions (`rw-------`).
const LOCK_FILE_MODE: u32 = 0o600;

const SOCKET_FILE_NAME: &str = "xgraph.sock";
const PID_FILE_NAME: &str = "daemon.pid";
const STARTUP_LOCK_FILE_NAME: &str = "startup.lock";
const DAEMON_LOCK_FILE_NAME: &str = "daemon.lock";

const RUNTIME_ROOT_NAME: &str = "xgraph";
const DEFAULT_RUNTIME_BASE: &str = "/tmp";
const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";

/// Errors produced by runtime path resolution and lock acquisition.
#[derive(Debug)]
pub enum RuntimeError {
    SocketPathTooLong {
        path: PathBuf,
        actual: usize,
        max: usize,
    },
    RuntimeDirPermissions {
        path: PathBuf,
        mode: u32,
        expected: u32,
    },
    RuntimeDirNotDirectory {
        path: PathBuf,
    },
    StartupLockHeld {
        path: PathBuf,
    },
    DaemonLockHeld {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketPathTooLong { path, actual, max } => write!(
                f,
                "Unix socket path {} is {actual} bytes including NUL; must be at most {max} bytes to fit sun_path",
                path.display()
            ),
            Self::RuntimeDirPermissions {
                path,
                mode,
                expected,
            } => write!(
                f,
                "runtime directory {} has mode {mode:#o}, expected {expected:#o}",
                path.display()
            ),
            Self::RuntimeDirNotDirectory { path } => {
                write!(
                    f,
                    "runtime path {} exists but is not a directory",
                    path.display()
                )
            }
            Self::StartupLockHeld { path } => {
                write!(f, "startup lock at {} is already held", path.display())
            }
            Self::DaemonLockHeld { path } => {
                write!(f, "daemon lock at {} is already held", path.display())
            }
            Self::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Runtime directory layout for a single worktree.
#[derive(Debug, Clone)]
pub struct RuntimeDir {
    dir: PathBuf,
}

impl RuntimeDir {
    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.dir.join(SOCKET_FILE_NAME)
    }

    pub fn pid_file_path(&self) -> PathBuf {
        self.dir.join(PID_FILE_NAME)
    }

    pub fn startup_lock_path(&self) -> PathBuf {
        self.dir.join(STARTUP_LOCK_FILE_NAME)
    }

    pub fn daemon_lock_path(&self) -> PathBuf {
        self.dir.join(DAEMON_LOCK_FILE_NAME)
    }

    pub fn as_path(&self) -> &Path {
        &self.dir
    }
}

/// RAII guard for the startup lock. Releases the kernel `flock` on drop.
/// The lock file itself is intentionally left in place.
#[derive(Debug)]
pub struct StartupLockGuard {
    file: Option<File>,
    path: PathBuf,
}

impl StartupLockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StartupLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

/// RAII guard for the daemon lock. Releases the kernel `flock` on drop.
/// The lock file itself is intentionally left in place.
#[derive(Debug)]
pub struct DaemonLockGuard {
    file: Option<File>,
    path: PathBuf,
}

impl DaemonLockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

/// Compute the lowercase hex SHA-256 of the canonical worktree root's path bytes.
pub fn worktree_root_hash(canonical_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_root.as_os_str().as_encoded_bytes());
    hex::encode(hasher.finalize())
}

/// Compute the runtime directory for the given canonical worktree root.
///
/// Layout: `<base>/xgraph/<hash>/` where `<base>` is the first existing
/// path in this preference order:
///
/// 1. `/run/user/$UID` — the systemd-managed per-user runtime dir. UID
///    is read from `/proc/self`'s file owner so we don't depend on
///    environment variables that the spawning process may or may not
///    have set.
/// 2. `$XDG_RUNTIME_DIR` if explicitly set (sandbox / container override).
/// 3. `/tmp` (final fallback).
///
/// The deliberate choice to prefer `/run/user/$UID` over the env var is
/// what keeps a daemon spawned by an LLM CLI (which typically doesn't
/// inherit XDG_RUNTIME_DIR) reachable from a normal user shell (which
/// usually has it set to the same `/run/user/$UID`). Without this, the
/// two would disagree and the CLI's `xgraph status` would report
/// "daemon socket: absent" even though the MCP-spawned daemon was
/// running fine.
///
/// Validates that the resulting socket path fits within Linux's `sun_path`
/// capacity, returning [`RuntimeError::SocketPathTooLong`] otherwise.
pub fn runtime_dir(canonical_root: &Path) -> Result<RuntimeDir, RuntimeError> {
    let base = default_runtime_base();
    runtime_dir_with_base(&base, canonical_root)
}

/// Resolve the per-user runtime base directory. See [`runtime_dir`] for
/// the preference order; this is the function it delegates to.
fn default_runtime_base() -> PathBuf {
    if let Some(uid) = current_uid() {
        let candidate = PathBuf::from(format!("/run/user/{uid}"));
        if candidate.is_dir() {
            return candidate;
        }
    }
    if let Some(raw) = std::env::var_os(XDG_RUNTIME_DIR_ENV)
        && !raw.is_empty()
    {
        return PathBuf::from(raw);
    }
    PathBuf::from(DEFAULT_RUNTIME_BASE)
}

/// Read the current uid from `/proc/self`'s file ownership. Returns
/// `None` only on systems without procfs, where the caller falls back
/// to `$XDG_RUNTIME_DIR` / `/tmp`.
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

fn runtime_dir_with_base(base: &Path, canonical_root: &Path) -> Result<RuntimeDir, RuntimeError> {
    let dir = base
        .join(RUNTIME_ROOT_NAME)
        .join(worktree_root_hash(canonical_root));
    validate_socket_path_length(&dir.join(SOCKET_FILE_NAME))?;
    Ok(RuntimeDir::new(dir))
}

/// Ensure the runtime directory exists with owner-only permissions.
///
/// If the directory does not exist it is created (along with the `xgraph/`
/// parent) with mode `0o700`. If it already exists, its permissions are
/// validated and any other mode is rejected.
pub fn ensure_runtime_dir(canonical_root: &Path) -> Result<RuntimeDir, RuntimeError> {
    let runtime = runtime_dir(canonical_root)?;
    ensure_dir(runtime.as_path())?;
    Ok(runtime)
}

#[cfg(test)]
fn ensure_runtime_dir_with_base(
    base: &Path,
    canonical_root: &Path,
) -> Result<RuntimeDir, RuntimeError> {
    let runtime = runtime_dir_with_base(base, canonical_root)?;
    ensure_dir(runtime.as_path())?;
    Ok(runtime)
}

fn ensure_dir(dir: &Path) -> Result<(), RuntimeError> {
    match std::fs::metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(RuntimeError::RuntimeDirNotDirectory {
                    path: dir.to_path_buf(),
                });
            }
            let mode = meta.permissions().mode() & 0o777;
            if mode != RUNTIME_DIR_MODE {
                return Err(RuntimeError::RuntimeDirPermissions {
                    path: dir.to_path_buf(),
                    mode,
                    expected: RUNTIME_DIR_MODE,
                });
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => create_runtime_dir(dir),
        Err(err) => Err(RuntimeError::Io {
            path: dir.to_path_buf(),
            source: err,
        }),
    }
}

fn create_runtime_dir(dir: &Path) -> Result<(), RuntimeError> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
            path: parent.to_path_buf(),
            source: err,
        })?;
    }

    DirBuilder::new()
        .mode(RUNTIME_DIR_MODE)
        .create(dir)
        .map_err(|err| RuntimeError::Io {
            path: dir.to_path_buf(),
            source: err,
        })?;

    Ok(())
}

fn validate_socket_path_length(path: &Path) -> Result<(), RuntimeError> {
    let bytes_with_nul = path.as_os_str().as_encoded_bytes().len() + 1;
    if bytes_with_nul > SUN_PATH_CAPACITY {
        return Err(RuntimeError::SocketPathTooLong {
            path: path.to_path_buf(),
            actual: bytes_with_nul,
            max: SUN_PATH_CAPACITY,
        });
    }
    Ok(())
}

/// Attempt to acquire the startup lock non-blocking.
///
/// Returns [`RuntimeError::StartupLockHeld`] if another process already holds
/// it. The returned guard releases the lock on drop; the lock file itself is
/// left in place.
pub fn acquire_startup_lock(runtime: &RuntimeDir) -> Result<StartupLockGuard, RuntimeError> {
    let path = runtime.startup_lock_path();
    let file = open_lock_file(&path)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(StartupLockGuard {
            file: Some(file),
            path,
        }),
        Err(err) if is_would_block(&err) => Err(RuntimeError::StartupLockHeld { path }),
        Err(err) => Err(RuntimeError::Io { path, source: err }),
    }
}

/// Attempt to acquire the daemon lock non-blocking.
///
/// Returns [`RuntimeError::DaemonLockHeld`] if another process already holds
/// it. The returned guard releases the lock on drop; the lock file itself is
/// left in place. The daemon is expected to hold this guard for its entire
/// lifetime.
pub fn acquire_daemon_lock(runtime: &RuntimeDir) -> Result<DaemonLockGuard, RuntimeError> {
    let path = runtime.daemon_lock_path();
    let file = open_lock_file(&path)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(DaemonLockGuard {
            file: Some(file),
            path,
        }),
        Err(err) if is_would_block(&err) => Err(RuntimeError::DaemonLockHeld { path }),
        Err(err) => Err(RuntimeError::Io { path, source: err }),
    }
}

fn open_lock_file(path: &Path) -> Result<File, RuntimeError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(LOCK_FILE_MODE)
        .open(path)
        .map_err(|err| RuntimeError::Io {
            path: path.to_path_buf(),
            source: err,
        })
}

fn is_would_block(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::ResourceBusy
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Build a short unique identifier suitable for embedding in filesystem
    /// paths that must fit within Linux `sun_path` limits. Combines a
    /// process-id and per-process counter for uniqueness across concurrent
    /// test runners and parallel tests.
    fn short_unique() -> String {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        format!("{pid:x}-{unique:x}")
    }

    struct TempBase {
        path: PathBuf,
    }

    impl TempBase {
        fn new() -> Self {
            // Keep the base path short so the full socket path fits within
            // SUN_PATH_CAPACITY across all tests.
            let path = PathBuf::from(format!("/tmp/xgt{}", short_unique()));
            std::fs::create_dir_all(&path).expect("create temp base directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempBase {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn unique_worktree_root() -> PathBuf {
        // Worktree root paths are only hashed; they do not need to exist.
        PathBuf::from(format!("/tmp/wt-{}", short_unique()))
    }

    #[test]
    fn hash_is_deterministic_for_same_path() {
        let root = PathBuf::from("/tmp/example/worktree");
        assert_eq!(worktree_root_hash(&root), worktree_root_hash(&root));
    }

    #[test]
    fn hash_differs_for_different_paths() {
        let a = PathBuf::from("/tmp/example/worktree-a");
        let b = PathBuf::from("/tmp/example/worktree-b");
        assert_ne!(worktree_root_hash(&a), worktree_root_hash(&b));
    }

    #[test]
    fn hash_is_lowercase_hex_64_chars() {
        let hash = worktree_root_hash(Path::new("/tmp/example"));
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn runtime_dir_layout_matches_spec() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let dir = runtime_dir_with_base(base.path(), &canonical).expect("runtime_dir");
        let expected = base
            .path()
            .join(RUNTIME_ROOT_NAME)
            .join(worktree_root_hash(&canonical));
        assert_eq!(dir.as_path(), expected.as_path());
        assert_eq!(
            dir.socket_path().file_name().and_then(|s| s.to_str()),
            Some(SOCKET_FILE_NAME)
        );
        assert_eq!(
            dir.pid_file_path().file_name().and_then(|s| s.to_str()),
            Some(PID_FILE_NAME)
        );
        assert_eq!(
            dir.startup_lock_path().file_name().and_then(|s| s.to_str()),
            Some(STARTUP_LOCK_FILE_NAME)
        );
        assert_eq!(
            dir.daemon_lock_path().file_name().and_then(|s| s.to_str()),
            Some(DAEMON_LOCK_FILE_NAME)
        );
    }

    #[test]
    fn socket_path_at_capacity_succeeds() {
        // Build a base path that places the socket exactly at SUN_PATH_CAPACITY.
        // Components: <base>/xgraph/<64-hex>/xgraph.sock plus NUL byte.
        let canonical = PathBuf::from("/x");
        let hash_len = worktree_root_hash(&canonical).len();
        let tail = format!("/{RUNTIME_ROOT_NAME}/").len() + hash_len + 1 + SOCKET_FILE_NAME.len();
        let nul = 1usize;
        let base_len = SUN_PATH_CAPACITY - tail - nul;
        let base_path = PathBuf::from(format!("/{}", "b".repeat(base_len - 1)));
        runtime_dir_with_base(&base_path, &canonical).expect("at capacity should succeed");
    }

    #[test]
    fn socket_path_one_over_capacity_fails() {
        let canonical = PathBuf::from("/x");
        let hash_len = worktree_root_hash(&canonical).len();
        let tail = format!("/{RUNTIME_ROOT_NAME}/").len() + hash_len + 1 + SOCKET_FILE_NAME.len();
        let nul = 1usize;
        // One byte too long.
        let base_len = SUN_PATH_CAPACITY - tail - nul + 1;
        let base_path = PathBuf::from(format!("/{}", "b".repeat(base_len - 1)));
        let err =
            runtime_dir_with_base(&base_path, &canonical).expect_err("expected SocketPathTooLong");
        match err {
            RuntimeError::SocketPathTooLong { actual, max, .. } => {
                assert_eq!(max, SUN_PATH_CAPACITY);
                assert_eq!(actual, SUN_PATH_CAPACITY + 1);
            }
            other => panic!("expected SocketPathTooLong, got {other:?}"),
        }
    }

    #[test]
    fn ensure_runtime_dir_creates_with_mode_0o700() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let runtime = ensure_runtime_dir_with_base(base.path(), &canonical).expect("ensure");
        let meta = std::fs::metadata(runtime.as_path()).expect("metadata");
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, RUNTIME_DIR_MODE);
    }

    #[test]
    fn ensure_runtime_dir_rejects_existing_with_wrong_permissions() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let runtime = runtime_dir_with_base(base.path(), &canonical).expect("runtime_dir");
        std::fs::create_dir_all(runtime.as_path()).expect("create_dir_all");
        std::fs::set_permissions(runtime.as_path(), Permissions::from_mode(0o755))
            .expect("set perms");
        let err = ensure_runtime_dir_with_base(base.path(), &canonical)
            .expect_err("expected permission error");
        match err {
            RuntimeError::RuntimeDirPermissions { mode, expected, .. } => {
                assert_eq!(mode, 0o755);
                assert_eq!(expected, RUNTIME_DIR_MODE);
            }
            other => panic!("expected RuntimeDirPermissions, got {other:?}"),
        }
    }

    #[test]
    fn ensure_runtime_dir_rejects_existing_non_directory() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let runtime = runtime_dir_with_base(base.path(), &canonical).expect("runtime_dir");
        if let Some(parent) = runtime.as_path().parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(runtime.as_path(), b"not a directory").expect("write file");
        let err = ensure_runtime_dir_with_base(base.path(), &canonical)
            .expect_err("expected non-directory error");
        assert!(
            matches!(err, RuntimeError::RuntimeDirNotDirectory { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn ensure_runtime_dir_accepts_existing_with_correct_mode() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let first = ensure_runtime_dir_with_base(base.path(), &canonical).expect("first ensure");
        let second = ensure_runtime_dir_with_base(base.path(), &canonical).expect("second ensure");
        assert_eq!(first.as_path(), second.as_path());
    }

    #[test]
    fn startup_lock_is_exclusive_and_releases_on_drop() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let runtime = ensure_runtime_dir_with_base(base.path(), &canonical).expect("ensure");

        let first = acquire_startup_lock(&runtime).expect("first startup lock");
        let err = acquire_startup_lock(&runtime).expect_err("second should fail");
        assert!(
            matches!(err, RuntimeError::StartupLockHeld { .. }),
            "got {err:?}"
        );
        drop(first);

        let _second = acquire_startup_lock(&runtime).expect("second after drop");
        assert!(
            runtime.startup_lock_path().exists(),
            "lock file must remain on disk"
        );
    }

    #[test]
    fn daemon_lock_is_exclusive_and_releases_on_drop() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let runtime = ensure_runtime_dir_with_base(base.path(), &canonical).expect("ensure");

        let first = acquire_daemon_lock(&runtime).expect("first daemon lock");
        let err = acquire_daemon_lock(&runtime).expect_err("second should fail");
        assert!(
            matches!(err, RuntimeError::DaemonLockHeld { .. }),
            "got {err:?}"
        );
        drop(first);

        let _second = acquire_daemon_lock(&runtime).expect("second after drop");
        assert!(
            runtime.daemon_lock_path().exists(),
            "lock file must remain on disk"
        );
    }

    #[test]
    fn lock_files_persist_after_guard_drop() {
        let base = TempBase::new();
        let canonical = unique_worktree_root();
        let runtime = ensure_runtime_dir_with_base(base.path(), &canonical).expect("ensure");
        {
            let _startup = acquire_startup_lock(&runtime).expect("startup");
            let _daemon = acquire_daemon_lock(&runtime).expect("daemon");
        }
        assert!(runtime.startup_lock_path().exists());
        assert!(runtime.daemon_lock_path().exists());
    }
}
