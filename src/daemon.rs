//! Daemon lifecycle and request dispatch.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    fn handle(&self, conn: UnixStream) -> JoinHandle<()>;
}

pub struct EchoHandler;

impl ConnectionHandler for EchoHandler {
    fn handle(&self, mut conn: UnixStream) -> JoinHandle<()> {
        tokio::spawn(async move {
            let _ = conn.write_all(b"ok\n").await;
            let _ = conn.shutdown().await;
        })
    }
}

pub struct DaemonConfig {
    pub runtime_dir: PathBuf,
    pub socket_name: &'static str,
    pub handler: Arc<dyn ConnectionHandler>,
}

impl DaemonConfig {
    pub fn new(runtime_dir: PathBuf, handler: Arc<dyn ConnectionHandler>) -> Self {
        Self {
            runtime_dir,
            socket_name: DEFAULT_SOCKET_NAME,
            handler,
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
    let accept_task = tokio::spawn(accept_loop(listener, handler, shutdown_rx));

    Ok(DaemonHandle {
        daemon: Daemon {
            runtime_dir,
            socket_path,
            pid_path,
            lock_file,
        },
        accept_task: Some(accept_task),
        shutdown_tx,
    })
}

async fn accept_loop(
    listener: UnixListener,
    handler: Arc<dyn ConnectionHandler>,
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
                    let _task = handler.handle(stream);
                }
            }
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
