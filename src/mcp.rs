//! MCP proxy and daemon socket protocol.
//!
//! The proxy bridges an agent's MCP stdio to the per-worktree daemon's Unix
//! socket. It implements lazy daemon startup, framed JSON-RPC pass-through,
//! and clean shutdown when either end closes.
//!
//! Newline-delimited JSON is used as the wire framing for the first version.
//! Each message is a single line of UTF-8 JSON terminated by `\n`. LSP-style
//! `Content-Length` framing can be added later without changing the
//! orchestration logic in this module.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use fs2::{FileExt, lock_contended_error};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Instant, sleep, timeout};

/// Default file name for the daemon's Unix domain socket.
pub const DEFAULT_SOCKET_NAME: &str = "xgraph.sock";

/// File name of the cross-process startup lock.
pub const STARTUP_LOCK_NAME: &str = "startup.lock";

/// File name of the daemon's diagnostic PID file.
pub const PID_FILE_NAME: &str = "daemon.pid";

/// Total time the proxy is willing to wait for a daemon to start serving.
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time to wait when probing the socket to see if a daemon is alive.
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Initial delay between socket probes during daemon startup polling.
const SOCKET_POLL_INITIAL: Duration = Duration::from_millis(10);

/// Maximum delay between socket probes during daemon startup polling.
const SOCKET_POLL_MAX: Duration = Duration::from_millis(100);

/// Convenience alias for the boxed future returned by [`DaemonLauncher`].
pub type SpawnFuture<'a> = Pin<Box<dyn Future<Output = Result<(), McpError>> + Send + 'a>>;

/// Errors produced by the MCP proxy.
#[derive(Debug)]
pub enum McpError {
    /// I/O error talking to the daemon socket, stdio, or runtime files.
    Io(io::Error),
    /// Failed to acquire `startup.lock`.
    StartupLock(io::Error),
    /// The daemon launcher reported a failure.
    Launcher(String),
    /// The daemon did not start accepting connections within the timeout.
    StartupTimeout,
    /// The runtime directory did not exist when the proxy started.
    MissingRuntimeDir(PathBuf),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "mcp proxy I/O error: {err}"),
            Self::StartupLock(err) => write!(f, "could not acquire startup.lock: {err}"),
            Self::Launcher(msg) => write!(f, "daemon launcher failed: {msg}"),
            Self::StartupTimeout => write!(f, "daemon did not start within timeout"),
            Self::MissingRuntimeDir(path) => {
                write!(f, "runtime directory missing: {}", path.display())
            }
        }
    }
}

impl std::error::Error for McpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) | Self::StartupLock(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for McpError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Abstraction over spawning the daemon process.
///
/// Defined locally so this module does not depend on `crate::daemon`. The CLI
/// wires the real process spawn implementation; tests use stubs.
///
/// The method returns a boxed future to keep the trait dyn-compatible without
/// pulling in extra macro dependencies. Implementations typically write
/// `Box::pin(async move { ... })`.
pub trait DaemonLauncher: Send + Sync {
    /// Spawn the daemon for the proxy's runtime directory.
    ///
    /// Returning `Ok(())` means the spawn was initiated. The proxy still
    /// polls the socket until it becomes connectable, so the launcher does
    /// not need to block until the daemon is fully ready.
    fn spawn_daemon(&self) -> SpawnFuture<'_>;
}

/// Configuration for an `McpProxy` invocation.
#[derive(Clone)]
pub struct McpConfig {
    pub runtime_dir: PathBuf,
    pub socket_name: &'static str,
    pub daemon_launcher: Arc<dyn DaemonLauncher>,
}

impl McpConfig {
    pub fn new(runtime_dir: PathBuf, daemon_launcher: Arc<dyn DaemonLauncher>) -> Self {
        Self {
            runtime_dir,
            socket_name: DEFAULT_SOCKET_NAME,
            daemon_launcher,
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join(self.socket_name)
    }

    pub fn startup_lock_path(&self) -> PathBuf {
        self.runtime_dir.join(STARTUP_LOCK_NAME)
    }

    pub fn pid_path(&self) -> PathBuf {
        self.runtime_dir.join(PID_FILE_NAME)
    }
}

/// Encapsulates the proxy's lifecycle.
pub struct McpProxy {
    config: McpConfig,
}

impl McpProxy {
    pub fn new(config: McpConfig) -> Self {
        Self { config }
    }

    /// Connect to the daemon, starting it lazily if necessary.
    pub async fn connect(&self) -> Result<UnixStream, McpError> {
        ensure_runtime_dir(&self.config.runtime_dir)?;
        let socket_path = self.config.socket_path();

        if let Some(stream) = try_ping(&socket_path).await {
            return Ok(stream);
        }

        let lock_path = self.config.startup_lock_path();
        match StartupLockGuard::try_acquire(&lock_path)? {
            Some(_guard) => {
                if let Some(stream) = try_ping(&socket_path).await {
                    return Ok(stream);
                }

                remove_if_exists(&socket_path)?;
                remove_if_exists(&self.config.pid_path())?;

                self.config.daemon_launcher.spawn_daemon().await?;
                wait_for_socket(&socket_path, DAEMON_STARTUP_TIMEOUT).await
            }
            None => wait_for_socket(&socket_path, DAEMON_STARTUP_TIMEOUT).await,
        }
    }

    /// Run the full proxy lifecycle against the supplied stdio streams.
    ///
    /// Sessions are bounded by the daemon connection. If the daemon
    /// dies mid-session (e.g. an `xgraph reindex` killed it to take
    /// the Cozo lock), the proxy lazy-spawns a fresh daemon and
    /// resumes pumping rather than propagating the closure up to the
    /// LLM CLI as "Transport closed".
    pub async fn proxy<R, W>(&self, stdin: R, stdout: W) -> Result<(), McpError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut stdin = BufReader::new(stdin);
        let mut stdout = stdout;
        loop {
            let stream = self.connect().await?;
            let (socket_reader, socket_writer) = stream.into_split();
            match mcp_pump_session(&mut stdin, &mut stdout, socket_reader, socket_writer).await? {
                PumpOutcome::ClientClosed => return Ok(()),
                PumpOutcome::DaemonClosed => {
                    eprintln!("xgraph mcp: daemon socket closed, reconnecting…");
                    continue;
                }
            }
        }
    }
}

/// Why a [`mcp_pump_session`] returned. Used by [`McpProxy::proxy`] to
/// decide whether to reconnect (daemon went away — usually because
/// `xgraph reindex` killed it) or exit (client closed stdin).
enum PumpOutcome {
    /// stdin reached EOF — the LLM CLI is done with us, exit cleanly.
    ClientClosed,
    /// The daemon socket closed mid-session. The outer loop should
    /// lazy-spawn a fresh daemon and start a new session.
    DaemonClosed,
}

/// Top-level entry point invoked by the CLI.
pub async fn run(config: McpConfig) -> Result<ExitCode, McpError> {
    let proxy = McpProxy::new(config);
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    proxy.proxy(stdin, stdout).await?;
    Ok(ExitCode::SUCCESS)
}

fn ensure_runtime_dir(dir: &Path) -> Result<(), McpError> {
    if !dir.exists() {
        return Err(McpError::MissingRuntimeDir(dir.to_path_buf()));
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), McpError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(McpError::Io(err)),
    }
}

async fn try_ping(socket_path: &Path) -> Option<UnixStream> {
    match timeout(SOCKET_PROBE_TIMEOUT, UnixStream::connect(socket_path)).await {
        Ok(Ok(stream)) => Some(stream),
        _ => None,
    }
}

async fn wait_for_socket(socket_path: &Path, total: Duration) -> Result<UnixStream, McpError> {
    let deadline = Instant::now() + total;
    let mut delay = SOCKET_POLL_INITIAL;
    loop {
        if let Some(stream) = try_ping(socket_path).await {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            return Err(McpError::StartupTimeout);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        sleep(delay.min(remaining)).await;
        delay = (delay * 2).min(SOCKET_POLL_MAX);
    }
}

/// RAII guard for `startup.lock`.
///
/// Acquires an exclusive file lock that is released when the guard drops or
/// when the process exits. Failures during release are best-effort; the
/// kernel will free the lock on process exit either way.
struct StartupLockGuard {
    file: Option<File>,
}

impl StartupLockGuard {
    /// Attempt to take the exclusive lock without blocking.
    ///
    /// Returns `Ok(Some(guard))` if this caller now owns the lock,
    /// `Ok(None)` if another process holds it, and `Err` only on real I/O
    /// failures opening the file.
    fn try_acquire(path: &Path) -> Result<Option<Self>, McpError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(McpError::StartupLock)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file: Some(file) })),
            Err(err) if is_contended(&err) => Ok(None),
            Err(err) => Err(McpError::StartupLock(err)),
        }
    }
}

fn is_contended(err: &io::Error) -> bool {
    let canonical = lock_contended_error();
    err.kind() == canonical.kind() && err.raw_os_error() == canonical.raw_os_error()
}

impl Drop for StartupLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

/// Newline-delimited JSON pump with MCP protocol translation.
///
/// Reads one JSON-RPC line from the client (LLM CLI), classifies it via
/// [`crate::mcp_protocol::classify_request`], and either answers
/// locally (initialize / tools/list / ping / notification ack) or
/// forwards a translated payload to the daemon and writes back the
/// (possibly MCP-wrapped) response.
///
/// Sequential — one outstanding daemon request at a time. That matches
/// how every MCP client we care about (Claude, Codex) drives a server,
/// and it lets us skip a request-id correlation table.
/// Run a single proxy session bound to a particular daemon socket.
/// Returns when either stdin closes (`ClientClosed`) or the daemon
/// socket closes (`DaemonClosed`). The caller — [`McpProxy::proxy`] —
/// handles reconnect on the daemon-close case.
async fn mcp_pump_session<R, W, SR, SW>(
    stdin_reader: &mut BufReader<R>,
    stdout: &mut W,
    socket_reader: SR,
    socket_writer: SW,
) -> Result<PumpOutcome, McpError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
    SR: AsyncRead + Unpin + Send + 'static,
    SW: AsyncWrite + Unpin + Send + 'static,
{
    use crate::mcp_protocol::{Action, classify_request, shape_outgoing};
    let mut socket_reader = BufReader::new(socket_reader);
    let mut socket_writer = socket_writer;

    let mut request_line = String::new();
    let mut daemon_response_line = String::new();
    loop {
        request_line.clear();
        // Race stdin against the daemon socket. While we're idle (no
        // pending forwarded request), the daemon should not send any
        // unsolicited bytes — so `fill_buf` only resolves on EOF or
        // unexpected data, both of which mean the daemon connection is
        // unusable.
        tokio::select! {
            biased;
            res = stdin_reader.read_line(&mut request_line) => {
                let bytes = res?;
                if bytes == 0 {
                    // Client closed stdin → tear the daemon connection
                    // down so the daemon's last-disconnect logic can fire.
                    let _ = socket_writer.shutdown().await;
                    return Ok(PumpOutcome::ClientClosed);
                }
            }
            _ = socket_reader.fill_buf() => {
                // Daemon closed (or sent unsolicited data, treated the
                // same — the protocol doesn't permit it).
                return Ok(PumpOutcome::DaemonClosed);
            }
        }
        match classify_request(&request_line) {
            Action::NoReply => continue,
            Action::Drop => {
                eprintln!("xgraph mcp: dropped malformed JSON-RPC line");
                continue;
            }
            Action::LocalReply(out_line) => {
                stdout.write_all(out_line.as_bytes()).await?;
                stdout.flush().await?;
            }
            Action::Forward { line, wrap_in_mcp } => {
                socket_writer.write_all(line.as_bytes()).await?;
                socket_writer.flush().await?;
                daemon_response_line.clear();
                let bytes = socket_reader.read_line(&mut daemon_response_line).await?;
                if bytes == 0 {
                    // Daemon closed mid-request. Treat as DaemonClosed
                    // so the outer loop reconnects and the LLM can
                    // retry the request against the fresh daemon.
                    return Ok(PumpOutcome::DaemonClosed);
                }
                let out_line = shape_outgoing(&daemon_response_line, wrap_in_mcp);
                stdout.write_all(out_line.as_bytes()).await?;
                stdout.flush().await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::time::{sleep, timeout};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "xgraph-mcp-{}-{}-{}",
                label,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock should be after Unix epoch")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct EchoDaemon {
        handle: tokio::task::JoinHandle<()>,
    }

    impl EchoDaemon {
        async fn bind(socket_path: &Path) -> Self {
            let listener = UnixListener::bind(socket_path).expect("bind listener");
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(serve_echo(stream));
                }
            });
            Self { handle }
        }

        fn abort(self) {
            self.handle.abort();
        }
    }

    async fn serve_echo(stream: UnixStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            let response = match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(mut value) => {
                    if let Some(obj) = value.as_object_mut() {
                        let id = obj.get("id").cloned().unwrap_or(serde_json::Value::Null);
                        obj.insert("echo_id".to_string(), id);
                    }
                    serde_json::to_string(&value).expect("serialize echo response")
                }
                Err(_) => format!("{{\"error\":\"bad json\",\"raw\":{trimmed:?}}}"),
            };
            let mut payload = response;
            payload.push('\n');
            if writer.write_all(payload.as_bytes()).await.is_err() {
                return;
            }
            if writer.flush().await.is_err() {
                return;
            }
        }
    }

    struct NoopLauncher;

    impl DaemonLauncher for NoopLauncher {
        fn spawn_daemon(&self) -> SpawnFuture<'_> {
            Box::pin(async {
                Err(McpError::Launcher(
                    "spawn should not be called when daemon is already running".into(),
                ))
            })
        }
    }

    struct CountingLauncher {
        calls: AtomicUsize,
        socket_path: PathBuf,
        delay: Duration,
        daemon: tokio::sync::Mutex<Option<EchoDaemon>>,
    }

    impl CountingLauncher {
        fn new(socket_path: PathBuf, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                socket_path,
                delay,
                daemon: tokio::sync::Mutex::new(None),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl DaemonLauncher for CountingLauncher {
        fn spawn_daemon(&self) -> SpawnFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let socket_path = self.socket_path.clone();
            let delay = self.delay;
            Box::pin(async move {
                sleep(delay).await;
                let daemon = EchoDaemon::bind(&socket_path).await;
                *self.daemon.lock().await = Some(daemon);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn proxies_request_to_existing_daemon() {
        let dir = TempDir::new("existing");
        let socket_path = dir.path.join(DEFAULT_SOCKET_NAME);
        let daemon = EchoDaemon::bind(&socket_path).await;

        let config = McpConfig::new(dir.path.clone(), Arc::new(NoopLauncher));
        let proxy = McpProxy::new(config);

        let (stdin_writer, stdin_reader) = duplex(4096);
        let (stdout_writer, mut stdout_reader) = duplex(4096);

        let proxy_task =
            tokio::spawn(async move { proxy.proxy(stdin_reader, stdout_writer).await });

        let mut stdin_writer = stdin_writer;
        // Use a non-MCP method name so the proxy's classifier falls
        // through to the raw-passthrough branch and our echo daemon
        // sees the request.
        stdin_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"some_custom_method\"}\n")
            .await
            .expect("write request");
        stdin_writer.flush().await.expect("flush request");

        let mut buf = vec![0u8; 4096];
        let n = timeout(Duration::from_secs(2), stdout_reader.read(&mut buf))
            .await
            .expect("read response in time")
            .expect("read ok");
        let response = std::str::from_utf8(&buf[..n]).expect("utf8 response");
        assert!(response.contains("\"echo_id\":1"), "got: {response}");

        drop(stdin_writer);
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .expect("proxy shuts down")
            .expect("join ok")
            .expect("proxy ok");
        daemon.abort();
    }

    #[tokio::test]
    async fn lazy_startup_waits_for_socket() {
        let dir = TempDir::new("lazy");
        let socket_path = dir.path.join(DEFAULT_SOCKET_NAME);
        let launcher = CountingLauncher::new(socket_path.clone(), Duration::from_millis(100));

        let config = McpConfig::new(dir.path.clone(), launcher.clone());
        let proxy = McpProxy::new(config);

        let (stdin_writer, stdin_reader) = duplex(4096);
        let (stdout_writer, mut stdout_reader) = duplex(4096);

        let proxy_task =
            tokio::spawn(async move { proxy.proxy(stdin_reader, stdout_writer).await });

        let mut stdin_writer = stdin_writer;
        stdin_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"some_custom_method\"}\n")
            .await
            .expect("write");
        stdin_writer.flush().await.expect("flush");

        let mut buf = vec![0u8; 4096];
        let n = timeout(Duration::from_secs(3), stdout_reader.read(&mut buf))
            .await
            .expect("response timing")
            .expect("read ok");
        let response = std::str::from_utf8(&buf[..n]).expect("utf8");
        assert!(response.contains("\"echo_id\":7"), "got: {response}");
        assert_eq!(launcher.call_count(), 1);

        drop(stdin_writer);
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .expect("shutdown")
            .expect("join")
            .expect("ok");
    }

    #[tokio::test]
    async fn concurrent_proxies_spawn_daemon_once() {
        let dir = TempDir::new("once");
        let socket_path = dir.path.join(DEFAULT_SOCKET_NAME);
        let launcher = CountingLauncher::new(socket_path.clone(), Duration::from_millis(150));

        let proxy_count = 4;
        let mut tasks = Vec::with_capacity(proxy_count);
        for _ in 0..proxy_count {
            let launcher: Arc<dyn DaemonLauncher> = launcher.clone();
            let config = McpConfig::new(dir.path.clone(), launcher);
            let proxy = McpProxy::new(config);

            let task = tokio::spawn(async move {
                let stream = proxy.connect().await.expect("connect");
                drop(stream);
            });
            tasks.push(task);
        }

        for task in tasks {
            timeout(Duration::from_secs(5), task)
                .await
                .expect("task done")
                .expect("join");
        }

        assert_eq!(launcher.call_count(), 1);
    }

    #[tokio::test]
    async fn clean_shutdown_on_stdin_eof() {
        let dir = TempDir::new("eof");
        let socket_path = dir.path.join(DEFAULT_SOCKET_NAME);
        let daemon = EchoDaemon::bind(&socket_path).await;

        let config = McpConfig::new(dir.path.clone(), Arc::new(NoopLauncher));
        let proxy = McpProxy::new(config);

        let (stdin_writer, stdin_reader) = duplex(4096);
        let (stdout_writer, mut stdout_reader) = duplex(4096);

        let proxy_task =
            tokio::spawn(async move { proxy.proxy(stdin_reader, stdout_writer).await });

        let mut stdin_writer = stdin_writer;
        stdin_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"some_custom_method\"}\n")
            .await
            .expect("write");
        stdin_writer.flush().await.expect("flush");

        let mut buf = vec![0u8; 4096];
        let n = timeout(Duration::from_secs(2), stdout_reader.read(&mut buf))
            .await
            .expect("response")
            .expect("read");
        assert!(
            std::str::from_utf8(&buf[..n])
                .unwrap()
                .contains("\"echo_id\":42")
        );

        drop(stdin_writer);
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .expect("proxy should terminate within timeout")
            .expect("join")
            .expect("proxy ok");
        daemon.abort();
    }

    /// A daemon that accepts one connection and immediately closes it.
    async fn bind_close_after_accept(socket_path: &Path) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(socket_path).expect("bind listener");
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        })
    }

    /// Launcher that binds a fresh echo daemon the first (and only)
    /// time the proxy asks for one. Used by the reconnect test: the
    /// first daemon drops the socket; on reconnect, this launcher
    /// brings up an echo daemon so the proxy resumes serving.
    struct OneShotEchoLauncher {
        socket_path: PathBuf,
        daemon: tokio::sync::Mutex<Option<EchoDaemon>>,
    }

    impl DaemonLauncher for OneShotEchoLauncher {
        fn spawn_daemon(&self) -> SpawnFuture<'_> {
            let socket_path = self.socket_path.clone();
            Box::pin(async move {
                // The first daemon (bind_close_after_accept) bound the
                // socket and may have left the file in place even
                // though its task ended. Remove it so we can rebind.
                let _ = std::fs::remove_file(&socket_path);
                let daemon = EchoDaemon::bind(&socket_path).await;
                *self.daemon.lock().await = Some(daemon);
                Ok(())
            })
        }
    }

    /// End-to-end MCP handshake: the proxy must answer `initialize`
    /// locally (without round-tripping to the daemon), then answer
    /// `tools/list`, and finally forward `tools/call` to the daemon
    /// wrapped in the MCP shape.
    #[tokio::test]
    async fn full_mcp_handshake_against_echo_daemon() {
        let dir = TempDir::new("mcp-handshake");
        let socket_path = dir.path.join(DEFAULT_SOCKET_NAME);
        let daemon = EchoDaemon::bind(&socket_path).await;

        let config = McpConfig::new(dir.path.clone(), Arc::new(NoopLauncher));
        let proxy = McpProxy::new(config);

        let (stdin_writer, stdin_reader) = duplex(8192);
        let (stdout_writer, mut stdout_reader) = duplex(8192);

        let proxy_task =
            tokio::spawn(async move { proxy.proxy(stdin_reader, stdout_writer).await });

        let mut stdin_writer = stdin_writer;
        // 1) initialize — handled locally; daemon never sees it.
        stdin_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        stdin_writer.flush().await.unwrap();

        let mut reader = BufReader::new(&mut stdout_reader);
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["result"]["serverInfo"]["name"], "xgraph");

        // 2) tools/list — also local.
        stdin_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        stdin_writer.flush().await.unwrap();
        line.clear();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 2);
        let tools = v["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "search"));

        // 3) tools/call → forwarded to daemon, daemon echoes back with
        //    echo_id, proxy wraps in MCP content shape.
        stdin_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"find_symbol\",\"arguments\":{\"name\":\"User\"}}}\n")
            .await
            .unwrap();
        stdin_writer.flush().await.unwrap();
        line.clear();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["result"]["isError"], false);
        // The wrapped content is a single text block whose text payload
        // is the daemon's `result` re-serialized — here our echo daemon
        // doesn't actually return a `result` key, so the text is "null".
        // The important assertion is the MCP shape.
        assert!(v["result"]["content"][0]["type"] == "text");

        drop(stdin_writer);
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        daemon.abort();
    }

    #[tokio::test]
    async fn proxy_reconnects_when_daemon_socket_closes() {
        let dir = TempDir::new("daemon-close");
        let socket_path = dir.path.join(DEFAULT_SOCKET_NAME);
        // First daemon: accepts our connection and immediately drops
        // it. The proxy should detect that, attempt reconnect via the
        // launcher, get a fresh daemon, and continue.
        let listener_task = bind_close_after_accept(&socket_path).await;

        // Reconnect target: a fresh echo daemon that the launcher
        // will bind once the proxy asks for it.
        let socket_path_for_launcher = socket_path.clone();
        let launcher: Arc<dyn DaemonLauncher> = Arc::new(OneShotEchoLauncher {
            socket_path: socket_path_for_launcher,
            daemon: tokio::sync::Mutex::new(None),
        });
        let config = McpConfig::new(dir.path.clone(), launcher);
        let proxy = McpProxy::new(config);

        let (stdin_writer, stdin_reader) = duplex(4096);
        let (stdout_writer, mut stdout_reader) = duplex(4096);

        let proxy_task =
            tokio::spawn(async move { proxy.proxy(stdin_reader, stdout_writer).await });

        // Send one request AFTER the first daemon closes — the proxy
        // should reconnect to the fresh echo daemon and the echo
        // response should come back to us on stdout.
        let mut stdin_writer = stdin_writer;
        // Give the proxy a moment to discover the daemon close and
        // start reconnecting before we feed the next request. The
        // exact timing doesn't matter — once the new daemon is bound
        // and our request arrives, the response will follow.
        sleep(Duration::from_millis(100)).await;
        stdin_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"some_custom_method\"}\n")
            .await
            .expect("write");
        stdin_writer.flush().await.expect("flush");

        let mut buf = vec![0u8; 4096];
        let n = timeout(Duration::from_secs(3), stdout_reader.read(&mut buf))
            .await
            .expect("reconnect should deliver a response within timeout")
            .expect("read");
        let response = std::str::from_utf8(&buf[..n]).expect("utf8");
        assert!(response.contains("\"echo_id\":11"), "got: {response}");

        drop(stdin_writer);
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .expect("proxy shuts down on stdin EOF")
            .expect("join")
            .expect("proxy ok");

        let _ = listener_task.await;
    }
}
