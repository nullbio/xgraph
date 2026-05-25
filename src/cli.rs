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
    /// Find a symbol by exact name.
    FindSymbol {
        name: String,
        #[arg(long)]
        kind: Option<String>,
    },
    /// Symbol search with optional `--prefix` / `--contains` mode and filters.
    Search {
        name: String,
        #[arg(long, value_enum, default_value_t = SearchMode::Exact)]
        mode: SearchMode,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long, default_value_t = 64)]
        limit: usize,
    },
    /// List callers of a node id.
    Callers { node_id: String },
    /// List callees of a node id.
    Callees { node_id: String },
    /// Transitive backward closure (calls / inherits / implements / references).
    Impact {
        node_id: String,
        #[arg(long, default_value_t = 0)]
        max_depth: u32,
    },
    /// Task context: symbol + source + callers + callees in one call.
    Context {
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 20)]
        related_limit: usize,
    },
    /// Shortest call path between two node ids.
    Trace {
        from: String,
        to: String,
        #[arg(long, default_value_t = 12)]
        max_depth: usize,
    },
    /// List all indexed files.
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SearchMode {
    Exact,
    Prefix,
    Contains,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum DaemonAction {
    /// Start the daemon manually.
    Start,
    /// Stop the daemon.
    Stop,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("failed to read current directory: {0}")]
    Cwd(std::io::Error),
    #[error(transparent)]
    GitDiscovery(#[from] GitDiscoveryError),
    #[error(transparent)]
    PersistentPaths(#[from] PersistentPathsError),
    #[error(transparent)]
    Cozo(#[from] crate::cozo::CozoError),
    #[error(transparent)]
    Ignore(#[from] IgnoreError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error("io error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Writer(#[from] crate::cozo::WriterError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Daemon(#[from] DaemonError),
    #[error(transparent)]
    Owner(#[from] crate::owner::OwnerError),
    #[error(transparent)]
    Mcp(#[from] crate::mcp::McpError),
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
        Command::FindSymbol { name, kind } => cmd_send_query(
            "find_symbol",
            serde_json::json!({
                "name": name,
                "kind": kind,
            }),
        ),
        Command::Search {
            name,
            mode,
            kind,
            path_prefix,
            limit,
        } => cmd_send_query(
            "search",
            serde_json::json!({
                "name": name,
                "mode": match mode {
                    SearchMode::Exact => "exact",
                    SearchMode::Prefix => "prefix",
                    SearchMode::Contains => "contains",
                },
                "kind": kind,
                "path_prefix": path_prefix,
                "limit": limit,
            }),
        ),
        Command::Callers { node_id } => {
            cmd_send_query("callers_of", serde_json::json!({ "node_id": node_id }))
        }
        Command::Callees { node_id } => {
            cmd_send_query("callees_of", serde_json::json!({ "node_id": node_id }))
        }
        Command::Impact { node_id, max_depth } => cmd_send_query(
            "impact",
            serde_json::json!({
                "node_id": node_id,
                "max_depth": max_depth,
            }),
        ),
        Command::Context {
            name,
            kind,
            related_limit,
        } => cmd_send_query(
            "context",
            serde_json::json!({
                "name": name,
                "kind": kind,
                "related_limit": related_limit,
            }),
        ),
        Command::Trace {
            from,
            to,
            max_depth,
        } => cmd_send_query(
            "trace",
            serde_json::json!({
                "from": from,
                "to": to,
                "max_depth": max_depth,
            }),
        ),
        Command::Files => cmd_send_query("files", serde_json::json!({})),
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
    let status = Arc::new(crate::daemon_status::DaemonStatus::new());
    let mut owner = WorktreeOwner::new(
        worktree.as_path().to_path_buf(),
        matcher,
        registry,
        store,
        indexes,
        status,
    )?;

    let progress = crate::progress::Progress::start();
    let summary = owner.index_all_with_progress(&progress)?;
    progress.stop();
    let errors = owner.shutdown();
    if let Some(first) = errors.into_iter().next() {
        return Err(CliError::Writer(first));
    }

    println!(
        "indexed {files} files: {nodes} nodes, {edges} edges into {dir}",
        files = summary.files_indexed,
        nodes = summary.nodes_created,
        edges = summary.edges_created,
        dir = persistent.root_dir().display(),
    );

    // If Claude / Codex are installed but xgraph isn't registered as an
    // MCP server with them, offer to add it. The check is silent when
    // both clients are absent or already configured. Non-interactive
    // sessions (CI, piped stdin) print a hint instead of prompting.
    let candidates = crate::mcp_install::clients_needing_install();
    if !candidates.is_empty()
        && let Err(err) = crate::mcp_install::prompt_and_install(&candidates)
    {
        eprintln!("xgraph: MCP install skipped: {err}");
    }
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
    let status = Arc::new(crate::daemon_status::DaemonStatus::new());
    // Keep a handle to the store for MCP read tools (impact, node, etc.).
    let store_for_handler = Arc::new(store.clone());
    let mut owner = WorktreeOwner::new(
        worktree.as_path().to_path_buf(),
        owner_matcher,
        registry,
        store,
        Arc::clone(&indexes),
        Arc::clone(&status),
    )?;

    let summary = owner.index_all()?;
    println!(
        "indexed {files} files ({nodes} nodes, {edges} edges); opening daemon socket at {sock}",
        files = summary.files_indexed,
        nodes = summary.nodes_created,
        edges = summary.edges_created,
        sock = runtime.socket_path().display(),
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

    let worktree_root_for_handler = worktree.as_path().to_path_buf();
    let result: Result<(), DaemonError> = runtime_tokio.block_on(async move {
        let handler = Arc::new(WorktreeHandler::new(
            indexes,
            status,
            worktree_root_for_handler,
            store_for_handler,
        ));
        let config = DaemonConfig::new(runtime.as_path().to_path_buf(), handler);
        let handle = crate::daemon::start(config).await?;
        let socket_path = handle.socket_path().to_path_buf();
        eprintln!("daemon listening on {}", socket_path.display());

        // Wake up on either an external signal (SIGTERM/SIGINT) OR the
        // daemon's own shutdown trigger — fired when the last client
        // connection closes. Either way we proceed to `handle.shutdown()`
        // which is idempotent.
        let mut shutdown_rx = handle.shutdown_subscriber();
        tokio::select! {
            res = wait_for_shutdown() => {
                if let Err(err) = res {
                    eprintln!("failed to install signal handler: {err}; shutting down");
                }
            }
            _ = async {
                loop {
                    if *shutdown_rx.borrow() {
                        return;
                    }
                    if shutdown_rx.changed().await.is_err() {
                        return;
                    }
                }
            } => {
                eprintln!("daemon: last client disconnected, exiting");
            }
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

/// Send a JSON-RPC request to the worktree's daemon socket and pretty-
/// print the response payload. Used by `search` / `context` / `callers`
/// / `callees` / `impact` / `trace` / `files` / `find-symbol` CLI
/// subcommands.
///
/// The daemon is the source of truth — running a query via CLI is just a
/// thin client over the same socket the MCP transport uses. No language
/// extractors are loaded in this process; all the work happens daemon-side.
fn cmd_send_query(method: &str, params: serde_json::Value) -> Result<ExitCode, CliError> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    let worktree = WorktreeRoot::discover(&cwd)?;
    let runtime = runtime_dir(worktree.as_path())?;
    let socket_path = runtime.socket_path();
    if !socket_path.exists() {
        eprintln!(
            "xgraph: daemon socket not found at {}. Start the daemon with `xgraph daemon start`.",
            socket_path.display()
        );
        return Ok(ExitCode::FAILURE);
    }

    let mut stream = UnixStream::connect(&socket_path).map_err(|source| CliError::Io {
        path: socket_path.clone(),
        source,
    })?;
    // Short read/write timeout so a hung daemon doesn't block the CLI.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut request_bytes = serde_json::to_vec(&request).expect("json serialize");
    request_bytes.push(b'\n');
    stream
        .write_all(&request_bytes)
        .map_err(|source| CliError::Io {
            path: socket_path.clone(),
            source,
        })?;
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|source| CliError::Io {
            path: socket_path.clone(),
            source,
        })?;
    let parsed: serde_json::Value = match serde_json::from_str(response.trim()) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("xgraph: malformed daemon response: {err}");
            eprintln!("raw: {response}");
            return Ok(ExitCode::FAILURE);
        }
    };
    if let Some(err) = parsed.get("error") {
        eprintln!(
            "xgraph: {}",
            err.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
        );
        return Ok(ExitCode::FAILURE);
    }
    let pretty = serde_json::to_string_pretty(&parsed.get("result").unwrap_or(&parsed))
        .unwrap_or_else(|_| parsed.to_string());
    println!("{pretty}");
    if let Some(meta) = parsed.get("meta")
        && let Some(catching_up) = meta.get("catching_up").and_then(|v| v.as_bool())
        && catching_up
    {
        eprintln!("note: daemon is catching up — result may be stale");
    }
    Ok(ExitCode::SUCCESS)
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
    fn parses_find_symbol_command() {
        let cli =
            parse(["xgraph", "find-symbol", "User", "--kind", "class"]).expect("parses");
        assert_eq!(
            cli.command,
            Command::FindSymbol {
                name: "User".to_string(),
                kind: Some("class".to_string()),
            }
        );
    }

    #[test]
    fn parses_search_command_with_prefix_mode() {
        let cli = parse(["xgraph", "search", "User", "--mode", "prefix"]).expect("parses");
        match cli.command {
            Command::Search {
                name, mode, limit, ..
            } => {
                assert_eq!(name, "User");
                assert!(matches!(mode, SearchMode::Prefix));
                assert_eq!(limit, 64);
            }
            other => panic!("expected Search, got {other:?}"),
        }
    }

    #[test]
    fn parses_callers_command() {
        let cli = parse(["xgraph", "callers", "h:42"]).expect("parses");
        assert_eq!(
            cli.command,
            Command::Callers {
                node_id: "h:42".to_string(),
            }
        );
    }

    #[test]
    fn parses_impact_command_with_max_depth() {
        let cli = parse(["xgraph", "impact", "h:42", "--max-depth", "5"]).expect("parses");
        assert_eq!(
            cli.command,
            Command::Impact {
                node_id: "h:42".to_string(),
                max_depth: 5,
            }
        );
    }

    #[test]
    fn parses_trace_command() {
        let cli = parse(["xgraph", "trace", "h:1", "h:2"]).expect("parses");
        assert_eq!(
            cli.command,
            Command::Trace {
                from: "h:1".to_string(),
                to: "h:2".to_string(),
                max_depth: 12,
            }
        );
    }

    #[test]
    fn parses_context_command() {
        let cli = parse(["xgraph", "context", "User"]).expect("parses");
        match cli.command {
            Command::Context { name, related_limit, .. } => {
                assert_eq!(name, "User");
                assert_eq!(related_limit, 20);
            }
            other => panic!("expected Context, got {other:?}"),
        }
    }

    #[test]
    fn parses_files_command() {
        let cli = parse(["xgraph", "files"]).expect("parses");
        assert_eq!(cli.command, Command::Files);
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
