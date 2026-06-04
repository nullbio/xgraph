//! Daemon lifecycle and request dispatch.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fs2::FileExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const DAEMON_LOCK_FILE: &str = "daemon.lock";
const DAEMON_PID_FILE: &str = "daemon.pid";
pub const DEFAULT_SOCKET_NAME: &str = "xgraph.sock";

/// Implementations spawn per-connection work and return the spawned task.
/// The accept loop does not await the returned handle, so handlers may run
/// indefinitely.
pub trait ConnectionHandler: Send + Sync {
    fn handle(&self, conn: UnixStream, activity: ActivityTracker) -> JoinHandle<()>;
}

pub struct EchoHandler;

impl ConnectionHandler for EchoHandler {
    fn handle(&self, mut conn: UnixStream, activity: ActivityTracker) -> JoinHandle<()> {
        tokio::spawn(async move {
            let _request = activity.begin_request();
            let _ = conn.write_all(b"ok\n").await;
            let _ = conn.shutdown().await;
        })
    }
}

#[derive(Clone)]
pub struct ActivityTracker {
    inner: Arc<Mutex<ActivityState>>,
}

struct ActivityState {
    active_requests: usize,
    last_activity: tokio::time::Instant,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ActivityState {
                active_requests: 0,
                last_activity: tokio::time::Instant::now(),
            })),
        }
    }

    pub fn begin_request(&self) -> ActivityGuard {
        let mut state = self.inner.lock().expect("activity tracker poisoned");
        state.active_requests += 1;
        state.last_activity = tokio::time::Instant::now();
        ActivityGuard {
            tracker: self.clone(),
        }
    }

    fn is_idle_for(&self, timeout: Duration) -> bool {
        let state = self.inner.lock().expect("activity tracker poisoned");
        state.active_requests == 0 && state.last_activity.elapsed() >= timeout
    }
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ActivityGuard {
    tracker: ActivityTracker,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let mut state = self
            .tracker
            .inner
            .lock()
            .expect("activity tracker poisoned");
        state.active_requests = state.active_requests.saturating_sub(1);
    }
}

#[derive(Clone, Debug)]
pub struct DaemonLifecycleConfig {
    pub idle_timeout: Option<Duration>,
    pub health_check_interval: Duration,
    pub worktree_root: Option<PathBuf>,
    pub persistent_root: Option<PathBuf>,
}

impl Default for DaemonLifecycleConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Some(Duration::from_secs(15 * 60)),
            health_check_interval: Duration::from_secs(30),
            worktree_root: None,
            persistent_root: None,
        }
    }
}

pub struct DaemonConfig {
    pub runtime_dir: PathBuf,
    pub socket_name: &'static str,
    pub handler: Arc<dyn ConnectionHandler>,
    pub lifecycle: DaemonLifecycleConfig,
}

impl DaemonConfig {
    pub fn new(runtime_dir: PathBuf, handler: Arc<dyn ConnectionHandler>) -> Self {
        Self {
            runtime_dir,
            socket_name: DEFAULT_SOCKET_NAME,
            handler,
            lifecycle: DaemonLifecycleConfig::default(),
        }
    }
}

pub struct Daemon {
    runtime_dir: PathBuf,
    socket_path: PathBuf,
    pid_path: PathBuf,
    lock_file: File,
}

impl Daemon {
    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn pid_path(&self) -> &Path {
        &self.pid_path
    }
}

#[derive(Debug)]
pub enum DaemonError {
    AlreadyRunning,
    Io(io::Error),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::AlreadyRunning => {
                f.write_str("another xgraph daemon already holds the runtime lock")
            }
            DaemonError::Io(err) => write!(f, "daemon I/O error: {err}"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DaemonError::AlreadyRunning => None,
            DaemonError::Io(err) => Some(err),
        }
    }
}

impl From<io::Error> for DaemonError {
    fn from(err: io::Error) -> Self {
        DaemonError::Io(err)
    }
}

pub struct DaemonHandle {
    daemon: Daemon,
    accept_task: Option<JoinHandle<()>>,
    lifecycle_task: Option<JoinHandle<()>>,
    shutdown_tx: watch::Sender<bool>,
}

impl DaemonHandle {
    pub fn socket_path(&self) -> &Path {
        self.daemon.socket_path()
    }

    pub fn pid_path(&self) -> &Path {
        self.daemon.pid_path()
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.daemon.runtime_dir
    }

    /// Subscribe to shutdown notifications. The receiver fires when shutdown
    /// is initiated externally via [`Self::shutdown`].
    /// Callers (e.g. `cmd_daemon_start`) await this to wake up and tear
    /// the runtime down once the accept loop has stopped.
    pub fn shutdown_subscriber(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Signal the accept loop to stop, await its exit, remove the socket
    /// and PID files (best effort), and release the daemon lock.
    pub async fn shutdown(mut self) -> Result<(), DaemonError> {
        let _ = self.shutdown_tx.send(true);

        if let Some(task) = self.accept_task.take() {
            match task.await {
                Ok(()) => {}
                Err(err) if err.is_cancelled() => {}
                Err(err) => {
                    return Err(DaemonError::Io(io::Error::other(format!(
                        "daemon accept task failed: {err}"
                    ))));
                }
            }
        }

        if let Some(task) = self.lifecycle_task.take() {
            task.abort();
            match task.await {
                Ok(()) => {}
                Err(err) if err.is_cancelled() => {}
                Err(err) => {
                    return Err(DaemonError::Io(io::Error::other(format!(
                        "daemon lifecycle task failed: {err}"
                    ))));
                }
            }
        }

        let _ = std::fs::remove_file(self.daemon.socket_path());
        let _ = std::fs::remove_file(self.daemon.pid_path());

        FileExt::unlock(&self.daemon.lock_file)?;

        Ok(())
    }
}

/// Start the daemon. Performs the README "Daemon startup" sequence:
/// acquire `daemon.lock`, remove any stale socket file, bind the Unix
/// listener, write a diagnostic PID file, then spawn the accept loop.
pub async fn start(config: DaemonConfig) -> Result<DaemonHandle, DaemonError> {
    let DaemonConfig {
        runtime_dir,
        socket_name,
        handler,
        lifecycle,
    } = config;

    std::fs::create_dir_all(&runtime_dir)?;

    let lock_path = runtime_dir.join(DAEMON_LOCK_FILE);
    let socket_path = runtime_dir.join(socket_name);
    let pid_path = runtime_dir.join(DAEMON_PID_FILE);

    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    match FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => {}
        Err(err) if err.kind() == fs2::lock_contended_error().kind() => {
            return Err(DaemonError::AlreadyRunning);
        }
        Err(err) => return Err(DaemonError::Io(err)),
    }

    match std::fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            let _ = FileExt::unlock(&lock_file);
            return Err(DaemonError::Io(err));
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            let _ = FileExt::unlock(&lock_file);
            return Err(DaemonError::Io(err));
        }
    };

    if let Err(err) = std::fs::write(&pid_path, format!("{}\n", std::process::id())) {
        let _ = std::fs::remove_file(&socket_path);
        let _ = FileExt::unlock(&lock_file);
        return Err(DaemonError::Io(err));
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let activity = ActivityTracker::new();
    let accept_task = tokio::spawn(accept_loop(
        listener,
        handler,
        activity.clone(),
        shutdown_rx,
    ));
    let lifecycle_task = tokio::spawn(lifecycle_loop(lifecycle, activity, shutdown_tx.clone()));

    Ok(DaemonHandle {
        daemon: Daemon {
            runtime_dir,
            socket_path,
            pid_path,
            lock_file,
        },
        accept_task: Some(accept_task),
        lifecycle_task: Some(lifecycle_task),
        shutdown_tx,
    })
}

async fn accept_loop(
    listener: UnixListener,
    handler: Arc<dyn ConnectionHandler>,
    activity: ActivityTracker,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accept = listener.accept() => {
                if let Ok((stream, _addr)) = accept {
                    std::mem::drop(handler.handle(stream, activity.clone()));
                }
            }
        }
    }
}

async fn lifecycle_loop(
    config: DaemonLifecycleConfig,
    activity: ActivityTracker,
    shutdown_tx: watch::Sender<bool>,
) {
    loop {
        tokio::time::sleep(config.health_check_interval).await;
        if *shutdown_tx.borrow() {
            return;
        }
        if config
            .worktree_root
            .as_deref()
            .is_some_and(|path| !path.exists())
            || config
                .persistent_root
                .as_deref()
                .is_some_and(|path| !path.exists())
            || config
                .idle_timeout
                .is_some_and(|timeout| activity.is_idle_for(timeout))
        {
            let _ = shutdown_tx.send(true);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixStream as ClientStream;

    fn config(runtime_dir: PathBuf) -> DaemonConfig {
        DaemonConfig::new(runtime_dir, Arc::new(EchoHandler))
    }

    async fn read_echo_line(path: &Path) -> String {
        let mut client = ClientStream::connect(path)
            .await
            .expect("client should connect to daemon socket");
        let mut buf = String::new();
        client
            .read_to_string(&mut buf)
            .await
            .expect("client should read echo response");
        buf
    }

    #[tokio::test]
    async fn start_creates_socket_and_pid_file() {
        let dir = TempDir::new().expect("tempdir");
        let handle = start(config(dir.path().to_path_buf()))
            .await
            .expect("daemon should start");

        assert!(handle.socket_path().exists(), "socket file should exist");
        assert!(handle.pid_path().exists(), "pid file should exist");

        handle.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn echo_handler_returns_ok_to_client() {
        let dir = TempDir::new().expect("tempdir");
        let handle = start(config(dir.path().to_path_buf()))
            .await
            .expect("daemon should start");

        let response = read_echo_line(handle.socket_path()).await;
        assert_eq!(response, "ok\n");

        handle.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn second_start_in_same_dir_reports_already_running() {
        let dir = TempDir::new().expect("tempdir");
        let first = start(config(dir.path().to_path_buf()))
            .await
            .expect("first daemon should start");

        let second = start(config(dir.path().to_path_buf())).await;
        match second {
            Err(DaemonError::AlreadyRunning) => {}
            Err(DaemonError::Io(err)) => panic!("expected AlreadyRunning, got Io({err})"),
            Ok(_handle) => panic!("expected AlreadyRunning, second start succeeded"),
        }

        first.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn shutdown_removes_socket_and_pid_file() {
        let dir = TempDir::new().expect("tempdir");
        let handle = start(config(dir.path().to_path_buf()))
            .await
            .expect("daemon should start");

        let socket = handle.socket_path().to_path_buf();
        let pid = handle.pid_path().to_path_buf();
        assert!(socket.exists());
        assert!(pid.exists());

        handle.shutdown().await.expect("shutdown should succeed");

        assert!(!socket.exists(), "socket file should be removed");
        assert!(!pid.exists(), "pid file should be removed");
    }

    #[tokio::test]
    async fn stale_socket_file_is_removed_at_startup() {
        let dir = TempDir::new().expect("tempdir");
        let socket_path = dir.path().join(DEFAULT_SOCKET_NAME);
        std::fs::write(&socket_path, b"stale").expect("write stale socket placeholder");
        assert!(socket_path.exists());

        let handle = start(config(dir.path().to_path_buf()))
            .await
            .expect("daemon should start over stale socket file");

        let response = read_echo_line(handle.socket_path()).await;
        assert_eq!(response, "ok\n");

        handle.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn daemon_stays_alive_when_last_connection_closes() {
        let dir = TempDir::new().expect("tempdir");
        let handle = start(config(dir.path().to_path_buf()))
            .await
            .expect("daemon should start");

        let mut shutdown_rx = handle.shutdown_subscriber();

        let _resp = read_echo_line(handle.socket_path()).await;

        let observed = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            shutdown_rx.changed().await
        })
        .await;
        assert!(
            observed.is_err(),
            "daemon must stay alive after the last client disconnects"
        );

        handle.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn daemon_shuts_down_after_idle_timeout_without_inflight_work() {
        let dir = TempDir::new().expect("tempdir");
        let mut cfg = config(dir.path().to_path_buf());
        cfg.lifecycle.idle_timeout = Some(std::time::Duration::from_millis(50));
        cfg.lifecycle.health_check_interval = std::time::Duration::from_millis(10);
        let handle = start(cfg).await.expect("daemon should start");

        let mut shutdown_rx = handle.shutdown_subscriber();
        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown_rx.changed())
            .await
            .expect("idle timeout should stop daemon")
            .expect("shutdown signal");
    }

    struct BlockingHandler {
        release: Arc<tokio::sync::Notify>,
    }

    impl ConnectionHandler for BlockingHandler {
        fn handle(&self, mut conn: ClientStream, activity: ActivityTracker) -> JoinHandle<()> {
            let release = Arc::clone(&self.release);
            tokio::spawn(async move {
                let _request = activity.begin_request();
                release.notified().await;
                let _ = conn.write_all(b"ok\n").await;
                let _ = conn.shutdown().await;
            })
        }
    }

    #[tokio::test]
    async fn daemon_does_not_idle_shutdown_while_request_is_in_flight() {
        let dir = TempDir::new().expect("tempdir");
        let release = Arc::new(tokio::sync::Notify::new());
        let mut cfg = DaemonConfig::new(
            dir.path().to_path_buf(),
            Arc::new(BlockingHandler {
                release: Arc::clone(&release),
            }),
        );
        cfg.lifecycle.idle_timeout = Some(std::time::Duration::from_millis(50));
        cfg.lifecycle.health_check_interval = std::time::Duration::from_millis(10);
        let handle = start(cfg).await.expect("daemon should start");
        let mut shutdown_rx = handle.shutdown_subscriber();
        let _client = ClientStream::connect(handle.socket_path())
            .await
            .expect("connect");

        let observed = tokio::time::timeout(std::time::Duration::from_millis(150), async {
            shutdown_rx.changed().await
        })
        .await;
        assert!(
            observed.is_err(),
            "daemon must not idle-timeout while a request is active"
        );

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown_rx.changed())
            .await
            .expect("idle timeout should fire after request completes")
            .expect("shutdown signal");
    }

    #[tokio::test]
    async fn daemon_shuts_down_when_worktree_root_disappears() {
        let runtime = TempDir::new().expect("runtime tempdir");
        let worktree = TempDir::new().expect("worktree tempdir");
        let persistent = worktree.path().join(".git").join("xgraph");
        std::fs::create_dir_all(&persistent).expect("persistent dir");
        let mut cfg = config(runtime.path().to_path_buf());
        cfg.lifecycle.worktree_root = Some(worktree.path().to_path_buf());
        cfg.lifecycle.persistent_root = Some(persistent);
        cfg.lifecycle.health_check_interval = std::time::Duration::from_millis(10);
        let handle = start(cfg).await.expect("daemon should start");
        let mut shutdown_rx = handle.shutdown_subscriber();

        std::fs::remove_dir_all(worktree.path()).expect("delete worktree root");

        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown_rx.changed())
            .await
            .expect("missing worktree should stop daemon")
            .expect("shutdown signal");
    }

    #[tokio::test]
    async fn daemon_shuts_down_when_persistent_root_disappears() {
        let runtime = TempDir::new().expect("runtime tempdir");
        let worktree = TempDir::new().expect("worktree tempdir");
        let persistent = worktree.path().join(".git").join("xgraph");
        std::fs::create_dir_all(&persistent).expect("persistent dir");
        let mut cfg = config(runtime.path().to_path_buf());
        cfg.lifecycle.worktree_root = Some(worktree.path().to_path_buf());
        cfg.lifecycle.persistent_root = Some(persistent.clone());
        cfg.lifecycle.health_check_interval = std::time::Duration::from_millis(10);
        let handle = start(cfg).await.expect("daemon should start");
        let mut shutdown_rx = handle.shutdown_subscriber();

        std::fs::remove_dir_all(persistent).expect("delete persistent root");

        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown_rx.changed())
            .await
            .expect("missing persistent root should stop daemon")
            .expect("shutdown signal");
    }

    #[tokio::test]
    async fn daemon_does_not_exit_before_any_client_connects() {
        let dir = TempDir::new().expect("tempdir");
        let handle = start(config(dir.path().to_path_buf()))
            .await
            .expect("daemon should start");

        // No clients have ever connected. The shutdown signal must
        // remain false for at least a short observation window.
        let mut shutdown_rx = handle.shutdown_subscriber();
        let observed = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            shutdown_rx.changed().await
        })
        .await;
        assert!(
            observed.is_err(),
            "daemon must not signal shutdown before any client has connected"
        );

        handle.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn shutdown_releases_lock_so_new_daemon_can_start() {
        let dir = TempDir::new().expect("tempdir");
        let first = start(config(dir.path().to_path_buf()))
            .await
            .expect("first daemon should start");
        first
            .shutdown()
            .await
            .expect("first shutdown should succeed");

        let second = start(config(dir.path().to_path_buf()))
            .await
            .expect("second daemon should start after first releases lock");
        second
            .shutdown()
            .await
            .expect("second shutdown should succeed");
    }
}
