//! Command-line interface for the `xgraph` binary.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::VERSION;
use crate::cozo::CozoStore;
use crate::daemon::{DaemonConfig, DaemonError};
use crate::git::{GitDiscoveryError, WorktreeRoot};
use crate::handlers::WorktreeHandler;
use crate::ignore::{IgnoreError, IgnoreMatcher};
use crate::language::LanguageRegistry;
use crate::owner::WorktreeOwner;
use crate::runtime::{RuntimeError, ensure_runtime_dir, runtime_dir};
use crate::scanner::ScanError;
use crate::storage::{PersistentPaths, PersistentPathsError};

#[derive(Debug, Parser)]
#[command(name = "xgraph", version = VERSION, about = "Linux-native code graph daemon for Git worktrees")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Initialize the Cozo schema, record project metadata, and run the initial scan.
    Init,
    /// Proxy MCP stdin/stdout to the worktree's daemon socket.
    Mcp,
    /// Daemon lifecycle commands.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Print daemon status and graph freshness.
    Status,
    /// Reconcile the manifest with files on disk.
    Sync,
    /// Rebuild the graph from scratch.
    Reindex,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum DaemonAction {
    /// Start the daemon manually.
    Start,
    /// Stop the daemon.
    Stop,
}

#[derive(Debug)]
pub enum CliError {
    Cwd(std::io::Error),
    GitDiscovery(GitDiscoveryError),
    PersistentPaths(PersistentPathsError),
    Cozo(crate::cozo::CozoError),
    Ignore(IgnoreError),
    Scan(ScanError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Writer(crate::cozo::WriterError),
    Runtime(RuntimeError),
    Daemon(DaemonError),
    Owner(crate::owner::OwnerError),
    Mcp(crate::mcp::McpError),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Cwd(err) => write!(f, "failed to read current directory: {err}"),
            CliError::GitDiscovery(err) => write!(f, "{err}"),
            CliError::PersistentPaths(err) => write!(f, "{err}"),
            CliError::Cozo(err) => write!(f, "{err}"),
            CliError::Ignore(err) => write!(f, "{err}"),
            CliError::Scan(err) => write!(f, "{err}"),
            CliError::Io { path, source } => write!(f, "io error on {}: {source}", path.display()),
            CliError::Writer(err) => write!(f, "{err}"),
            CliError::Runtime(err) => write!(f, "{err}"),
            CliError::Daemon(err) => write!(f, "{err}"),
            CliError::Owner(err) => write!(f, "{err}"),
            CliError::Mcp(err) => write!(f, "{err}"),
        }
    }
}

impl From<RuntimeError> for CliError {
    fn from(err: RuntimeError) -> Self {
        CliError::Runtime(err)
    }
}

impl From<DaemonError> for CliError {
    fn from(err: DaemonError) -> Self {
        CliError::Daemon(err)
    }
}

impl From<crate::owner::OwnerError> for CliError {
    fn from(err: crate::owner::OwnerError) -> Self {
        CliError::Owner(err)
    }
}

impl std::error::Error for CliError {}

impl From<GitDiscoveryError> for CliError {
    fn from(err: GitDiscoveryError) -> Self {
        CliError::GitDiscovery(err)
    }
}

impl From<PersistentPathsError> for CliError {
    fn from(err: PersistentPathsError) -> Self {
        CliError::PersistentPaths(err)
    }
}

impl From<crate::cozo::CozoError> for CliError {
    fn from(err: crate::cozo::CozoError) -> Self {
        CliError::Cozo(err)
    }
}

impl From<IgnoreError> for CliError {
    fn from(err: IgnoreError) -> Self {
        CliError::Ignore(err)
    }
}

impl From<ScanError> for CliError {
    fn from(err: ScanError) -> Self {
        CliError::Scan(err)
    }
}

impl From<crate::cozo::WriterError> for CliError {
    fn from(err: crate::cozo::WriterError) -> Self {
        CliError::Writer(err)
    }
}

pub fn run<I, S>(args: I) -> ExitCode
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    match parse(args) {
        Ok(cli) => match dispatch(cli) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("xgraph: {err}");
                ExitCode::FAILURE
            }
        },
        Err(err) => {
            let exit_code = if err.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
            let _ = err.print();
            exit_code
        }
    }
}

pub fn parse<I, S>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let owned = args.into_iter().map(Into::into).collect::<Vec<String>>();
    Cli::try_parse_from(owned)
}

pub fn dispatch(cli: Cli) -> Result<ExitCode, CliError> {
    match cli.command {
        Command::Init => cmd_init(),
        Command::Mcp => cmd_mcp(),
        Command::Daemon {
            action: DaemonAction::Start,
        } => cmd_daemon_start(),
        Command::Daemon {
            action: DaemonAction::Stop,
        } => cmd_daemon_stop(),
        Command::Status => cmd_status(),
        Command::Sync => cmd_sync(),
        Command::Reindex => cmd_reindex(),
    }
}

fn cmd_init() -> Result<ExitCode, CliError> {
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    init_at(&cwd)
}

pub fn init_at(start: &Path) -> Result<ExitCode, CliError> {
    let worktree = WorktreeRoot::discover(start)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    persistent.ensure_created()?;

    let store = CozoStore::open(&persistent.cozo_db_path())?;
    let matcher = IgnoreMatcher::new(worktree.as_path())?;
    let registry = LanguageRegistry::with_all();
    let indexes = Arc::new(crate::indexes::HotIndexes::new());
    let mut owner = WorktreeOwner::new(
        worktree.as_path().to_path_buf(),
        matcher,
        registry,
        store,
        indexes,
    )?;

    let indexed = owner.index_all()?;
    let errors = owner.shutdown();
    if let Some(first) = errors.into_iter().next() {
        return Err(CliError::Writer(first));
    }

    println!(
        "indexed {indexed} files into {}",
        persistent.root_dir().display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_mcp() -> Result<ExitCode, CliError> {
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    let worktree = WorktreeRoot::discover(&cwd)?;
    let runtime = ensure_runtime_dir(worktree.as_path())?;

    let launcher = Arc::new(SubprocessLauncher {
        worktree_root: worktree.as_path().to_path_buf(),
    });
    let config = crate::mcp::McpConfig::new(runtime.as_path().to_path_buf(), launcher);

    let runtime_tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| CliError::Io {
            path: PathBuf::from("<tokio runtime>"),
            source: err,
        })?;
    let exit_code = runtime_tokio
        .block_on(crate::mcp::run(config))
        .map_err(CliError::Mcp)?;
    Ok(exit_code)
}

struct SubprocessLauncher {
    worktree_root: PathBuf,
}

impl crate::mcp::DaemonLauncher for SubprocessLauncher {
    fn spawn_daemon(&self) -> crate::mcp::SpawnFuture<'_> {
        let exe = env::current_exe();
        let cwd = self.worktree_root.clone();
        Box::pin(async move {
            let exe_path = exe.map_err(crate::mcp::McpError::Io)?;
            let _ = std::process::Command::new(exe_path)
                .arg("daemon")
                .arg("start")
                .current_dir(&cwd)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null())
                .spawn()
                .map_err(crate::mcp::McpError::Io)?;
            Ok(())
        })
    }
}

fn cmd_daemon_start() -> Result<ExitCode, CliError> {
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    let worktree = WorktreeRoot::discover(&cwd)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    persistent.ensure_created()?;
    let runtime = ensure_runtime_dir(worktree.as_path())?;

    let store = CozoStore::open(&persistent.cozo_db_path())?;
    let registry = LanguageRegistry::with_all();
    // The watcher and the scanner share a matcher view of the worktree.
    let watcher_matcher = std::sync::Arc::new(IgnoreMatcher::new(worktree.as_path())?);
    let owner_matcher = IgnoreMatcher::new(worktree.as_path())?;

    // Hot indexes are loaded from Cozo first so any restart sees the prior
    // graph immediately; the owner then mirrors fresh updates into them.
    let indexes = Arc::new(crate::indexes::HotIndexes::load_from_cozo(&store)?);
    let mut owner = WorktreeOwner::new(
        worktree.as_path().to_path_buf(),
        owner_matcher,
        registry,
        store,
        Arc::clone(&indexes),
    )?;

    let indexed = owner.index_all()?;
    println!(
        "indexed {indexed} files; opening daemon socket at {}",
        runtime.socket_path().display()
    );

    // Start the watcher and hand its batches to an OS thread that owns the
    // WorktreeOwner. The thread loops until batch_rx closes (when the watcher
    // handle is dropped at daemon shutdown).
    let (watcher_handle, batch_rx) = crate::watcher::Watcher::start(
        worktree.as_path(),
        watcher_matcher,
        crate::watcher::WatcherConfig::default(),
    )
    .map_err(|err| CliError::Io {
        path: worktree.as_path().to_path_buf(),
        source: std::io::Error::other(err.to_string()),
    })?;

    let worktree_root_for_thread = worktree.as_path().to_path_buf();
    let watcher_thread = std::thread::Builder::new()
        .name("xgraph-watcher-handler".into())
        .spawn(move || {
            while let Ok(batch) = batch_rx.recv() {
                for path in batch.created.iter().chain(batch.modified.iter()) {
                    if let Err(err) = owner.process_change(path.clone()) {
                        eprintln!("watcher: process_change {}: {err}", path.display());
                    }
                }
                for path in &batch.deleted {
                    if let Err(err) = owner.process_delete(path.clone()) {
                        eprintln!("watcher: process_delete {}: {err}", path.display());
                    }
                }
                // worktree_root_for_thread is captured for the diagnostic logs above; the
                // strip_prefix happens inside process_delete itself.
                let _ = &worktree_root_for_thread;
                if batch.ignore_file_changed
                    && let Err(err) = owner.reconcile_after_ignore_change()
                {
                    eprintln!("watcher: ignore-change reconciliation failed: {err}");
                }
            }
            let _ = owner.shutdown();
        })
        .map_err(|source| CliError::Io {
            path: worktree.as_path().to_path_buf(),
            source,
        })?;

    let runtime_tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| CliError::Io {
            path: PathBuf::from("<tokio runtime>"),
            source: err,
        })?;

    let result: Result<(), DaemonError> = runtime_tokio.block_on(async move {
        let handler = Arc::new(WorktreeHandler::new(indexes));
        let config = DaemonConfig::new(runtime.as_path().to_path_buf(), handler);
        let handle = crate::daemon::start(config).await?;
        let socket_path = handle.socket_path().to_path_buf();
        eprintln!("daemon listening on {}", socket_path.display());
        // Wait for SIGTERM/SIGINT.
        if let Err(err) = wait_for_shutdown().await {
            eprintln!("failed to install signal handler: {err}; shutting down");
        }
        handle.shutdown().await
    });

    // Drop the watcher handle so the worker thread sees batch_rx close, then
    // join it (which also drains and shuts the owner down).
    drop(watcher_handle);
    let _ = watcher_thread.join();

    result?;
    Ok(ExitCode::SUCCESS)
}

async fn wait_for_shutdown() -> Result<(), std::io::Error> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}

fn cmd_daemon_stop() -> Result<ExitCode, CliError> {
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    let worktree = WorktreeRoot::discover(&cwd)?;
    let runtime = runtime_dir(worktree.as_path())?;
    let pid_path = runtime.pid_file_path();
    if !pid_path.exists() {
        println!("no daemon running for this worktree");
        return Ok(ExitCode::SUCCESS);
    }
    let pid_text = fs::read_to_string(&pid_path).map_err(|source| CliError::Io {
        path: pid_path.clone(),
        source,
    })?;
    let pid: i32 = pid_text.trim().parse().unwrap_or(0);
    if pid <= 0 {
        println!("daemon pid file is invalid; cleaning up");
        let _ = fs::remove_file(&pid_path);
        return Ok(ExitCode::SUCCESS);
    }
    // SIGTERM via libc; nix isn't a direct dep. Use std::process::Command kill -15 fallback.
    let status = std::process::Command::new("kill")
        .arg("-15")
        .arg(pid.to_string())
        .status();
    match status {
        Ok(s) if s.success() => println!("sent SIGTERM to daemon pid {pid}"),
        Ok(_) | Err(_) => println!("daemon pid {pid} no longer running"),
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_status() -> Result<ExitCode, CliError> {
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    let worktree = WorktreeRoot::discover(&cwd)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    let runtime = runtime_dir(worktree.as_path())?;
    let cozo_present = persistent.cozo_db_path().exists();
    let socket_path = runtime.socket_path();
    let socket_state = if !socket_path.exists() {
        "absent"
    } else {
        // `UnixStream::connect` blocks indefinitely on a hung socket. There's
        // no direct connect_timeout for AF_UNIX in std, so we run the connect
        // on a helper thread and join with a short deadline.
        let socket_path_for_probe = socket_path.clone();
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let ok = std::os::unix::net::UnixStream::connect(&socket_path_for_probe).is_ok();
            let _ = tx.send(ok);
        });
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(true) => "reachable",
            Ok(false) => "stale (file exists but no daemon accepting)",
            Err(_) => "stale (connect timed out)",
        }
    };
    let pid_present = runtime.pid_file_path().exists();
    println!("worktree:      {}", worktree.as_path().display());
    println!("persistent:    {}", persistent.root_dir().display());
    println!("runtime:       {}", runtime.as_path().display());
    println!(
        "graph:         {}",
        if cozo_present {
            "present"
        } else {
            "absent (run `xgraph init`)"
        }
    );
    println!("daemon socket: {socket_state}");
    println!(
        "daemon pid:    {}",
        if pid_present { "present" } else { "absent" }
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_sync() -> Result<ExitCode, CliError> {
    // Sync is idempotent: every file is re-hashed and the hash-skip cache
    // (introduced in Phase P3) keeps untouched files from being re-extracted.
    // Drift between disk and the active manifest is healed by the same
    // pipeline as init.
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    init_at(&cwd)
}

fn cmd_reindex() -> Result<ExitCode, CliError> {
    // Reindex truncates the graph relations and runs a full fresh scan.
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    let worktree = WorktreeRoot::discover(&cwd)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    persistent.ensure_created()?;
    let store = CozoStore::open(&persistent.cozo_db_path())?;
    store.truncate_graph()?;
    drop(store);
    init_at(&cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn parses_help_as_display_help_error() {
        let err = parse(["xgraph", "--help"]).expect_err("--help should return a clap error");
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
        assert!(!err.use_stderr());
    }

    #[test]
    fn parses_version_as_display_version_error() {
        let err = parse(["xgraph", "--version"]).expect_err("--version should return a clap error");
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert!(!err.use_stderr());
    }

    #[test]
    fn parses_init_command() {
        let cli = parse(["xgraph", "init"]).expect("init should parse");
        assert_eq!(cli.command, Command::Init);
    }

    #[test]
    fn parses_mcp_command() {
        let cli = parse(["xgraph", "mcp"]).expect("mcp should parse");
        assert_eq!(cli.command, Command::Mcp);
    }

    #[test]
    fn parses_daemon_start_command() {
        let cli = parse(["xgraph", "daemon", "start"]).expect("daemon start should parse");
        assert_eq!(
            cli.command,
            Command::Daemon {
                action: DaemonAction::Start
            }
        );
    }

    #[test]
    fn parses_daemon_stop_command() {
        let cli = parse(["xgraph", "daemon", "stop"]).expect("daemon stop should parse");
        assert_eq!(
            cli.command,
            Command::Daemon {
                action: DaemonAction::Stop
            }
        );
    }

    #[test]
    fn parses_status_command() {
        let cli = parse(["xgraph", "status"]).expect("status should parse");
        assert_eq!(cli.command, Command::Status);
    }

    #[test]
    fn parses_sync_command() {
        let cli = parse(["xgraph", "sync"]).expect("sync should parse");
        assert_eq!(cli.command, Command::Sync);
    }

    #[test]
    fn parses_reindex_command() {
        let cli = parse(["xgraph", "reindex"]).expect("reindex should parse");
        assert_eq!(cli.command, Command::Reindex);
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let err = parse(["xgraph", "unknown"]).expect_err("unknown subcommand should fail");
        assert!(matches!(
            err.kind(),
            ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument
        ));
    }

    #[test]
    fn rejects_missing_subcommand() {
        let err = parse(["xgraph"]).expect_err("missing subcommand should fail");
        assert!(matches!(
            err.kind(),
            ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }

    #[test]
    fn rejects_daemon_without_action() {
        let err = parse(["xgraph", "daemon"]).expect_err("daemon without action should fail");
        assert!(matches!(
            err.kind(),
            ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }
}
