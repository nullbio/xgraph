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
use crate::runtime::{
    RuntimeDir, RuntimeError, StartupLockGuard, acquire_daemon_lock, acquire_startup_lock,
    ensure_runtime_dir, runtime_dir,
};
use crate::scanner::ScanError;
use crate::storage::{PersistentPaths, PersistentPathsError};

#[derive(Debug, Parser)]
#[command(name = "xgraph", version = VERSION, about = "Linux-native code graph daemon for Git worktrees")]
pub struct Cli {
    /// Path inside the Git worktree to operate on. Defaults to the current directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub project_root: Option<PathBuf>,
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
    Callers {
        node_id: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// List callees of a node id.
    Callees {
        node_id: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Transitive backward closure (calls / inherits / implements / references).
    Impact {
        node_id: String,
        #[arg(long, default_value_t = 0)]
        max_depth: u32,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Task context: symbol + source + callers + callees in one call.
    Context {
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
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
    /// List indexed files.
    Files {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
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
    /// Stop the daemon. Sends SIGTERM by default; pass `--force` to
    /// send SIGKILL and clean up stale runtime files even if the
    /// daemon is wedged or the socket is unresponsive.
    Stop {
        #[arg(long)]
        force: bool,
    },
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
    let project_root = cli.project_root;
    match cli.command {
        Command::Init => cmd_init(project_root.as_deref()),
        Command::Mcp => cmd_mcp(),
        Command::Daemon {
            action: DaemonAction::Start,
        } => cmd_daemon_start(project_root.as_deref()),
        Command::Daemon {
            action: DaemonAction::Stop { force },
        } => cmd_daemon_stop(project_root.as_deref(), force),
        Command::Status => cmd_status(project_root.as_deref()),
        Command::Sync => cmd_sync(project_root.as_deref()),
        Command::Reindex => cmd_reindex(project_root.as_deref()),
        Command::FindSymbol { name, kind } => cmd_send_query(
            project_root.as_deref(),
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
            project_root.as_deref(),
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
        Command::Callers {
            node_id,
            limit,
            offset,
        } => cmd_send_query(
            project_root.as_deref(),
            "callers_of",
            serde_json::json!({ "node_id": node_id, "limit": limit, "offset": offset }),
        ),
        Command::Callees {
            node_id,
            limit,
            offset,
        } => cmd_send_query(
            project_root.as_deref(),
            "callees_of",
            serde_json::json!({ "node_id": node_id, "limit": limit, "offset": offset }),
        ),
        Command::Impact {
            node_id,
            max_depth,
            limit,
            offset,
        } => cmd_send_query(
            project_root.as_deref(),
            "impact",
            serde_json::json!({
                "node_id": node_id,
                "max_depth": max_depth,
                "limit": limit,
                "offset": offset,
            }),
        ),
        Command::Context {
            name,
            kind,
            path_prefix,
            limit,
            related_limit,
        } => cmd_send_query(
            project_root.as_deref(),
            "context",
            serde_json::json!({
                "name": name,
                "kind": kind,
                "path_prefix": path_prefix,
                "limit": limit,
                "related_limit": related_limit,
            }),
        ),
        Command::Trace {
            from,
            to,
            max_depth,
        } => cmd_send_query(
            project_root.as_deref(),
            "trace",
            serde_json::json!({
                "from": from,
                "to": to,
                "max_depth": max_depth,
            }),
        ),
        Command::Files {
            prefix,
            limit,
            offset,
        } => cmd_send_query(
            project_root.as_deref(),
            "files",
            serde_json::json!({
                "prefix": prefix,
                "limit": limit,
                "offset": offset,
            }),
        ),
    }
}

fn requested_worktree(project_root: Option<&Path>) -> Result<WorktreeRoot, CliError> {
    match project_root {
        Some(path) => Ok(WorktreeRoot::discover(path)?),
        None => {
            let cwd = env::current_dir().map_err(CliError::Cwd)?;
            Ok(WorktreeRoot::discover(&cwd)?)
        }
    }
}

fn cmd_init(project_root: Option<&Path>) -> Result<ExitCode, CliError> {
    let worktree = requested_worktree(project_root)?;
    cmd_init_at_worktree(&worktree)
}

#[cfg(test)]
fn cmd_init_at(start: &Path) -> Result<ExitCode, CliError> {
    let worktree = WorktreeRoot::discover(start)?;
    cmd_init_at_worktree(&worktree)
}

fn cmd_init_at_worktree(worktree: &WorktreeRoot) -> Result<ExitCode, CliError> {
    let persistent = PersistentPaths::for_worktree(worktree)?;
    if let Some(response) =
        send_daemon_request_if_reachable(worktree, "sync", serde_json::json!({}))?
    {
        if print_daemon_error_if_any(&response) {
            return Ok(ExitCode::FAILURE);
        }
        let result = response.get("result").unwrap_or(&response);
        print_daemon_index_summary(worktree, result, persistent.root_dir())?;
        maybe_prompt_mcp_install();
        return Ok(ExitCode::SUCCESS);
    }
    init_at_worktree(worktree, &persistent)
}

pub fn init_at(start: &Path) -> Result<ExitCode, CliError> {
    let worktree = WorktreeRoot::discover(start)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    init_at_worktree(&worktree, &persistent)
}

pub fn reindex_at(start: &Path) -> Result<ExitCode, CliError> {
    let worktree = WorktreeRoot::discover(start)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    reindex_at_worktree(&worktree, &persistent)
}

fn init_at_worktree(
    worktree: &WorktreeRoot,
    persistent: &PersistentPaths,
) -> Result<ExitCode, CliError> {
    persistent.ensure_created()?;
    let runtime = ensure_runtime_dir(worktree.as_path())?;
    let _startup_guard =
        acquire_startup_lock_with_retry(&runtime, std::time::Duration::from_secs(60))?;
    init_at_locked(worktree, persistent)
}

fn init_at_locked(
    worktree: &WorktreeRoot,
    persistent: &PersistentPaths,
) -> Result<ExitCode, CliError> {
    // Direct maintenance owns startup.lock while the daemon is stopped so MCP
    // proxies cannot lazy-spawn a replacement daemon that races us for the
    // Cozo store lock.
    ensure_no_running_daemon(worktree.as_path())?;

    let store = open_store_with_lock_retry(
        &persistent.cozo_db_path(),
        std::time::Duration::from_secs(60),
    )?;
    let store_for_counts = store.clone();
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
    let graph_counts = GraphCounts::from_indexes(&crate::indexes::HotIndexes::load_from_cozo(
        &store_for_counts,
    )?);
    print_index_summary_parts(
        summary.files_scanned as u64,
        summary.files_indexed as u64,
        summary.nodes_created,
        summary.edges_created,
        Some(graph_counts),
        persistent.root_dir(),
    );

    maybe_prompt_mcp_install();
    Ok(ExitCode::SUCCESS)
}

fn reindex_at_worktree(
    worktree: &WorktreeRoot,
    persistent: &PersistentPaths,
) -> Result<ExitCode, CliError> {
    persistent.ensure_created()?;
    let runtime = ensure_runtime_dir(worktree.as_path())?;
    let _startup_guard =
        acquire_startup_lock_with_retry(&runtime, std::time::Duration::from_secs(60))?;
    ensure_no_running_daemon(worktree.as_path())?;

    let store = open_store_with_lock_retry(
        &persistent.cozo_db_path(),
        std::time::Duration::from_secs(60),
    )?;
    let store_for_counts = store.clone();
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
    let summary = owner.reindex_all_with_progress(&progress)?;
    progress.stop();
    let errors = owner.shutdown();
    if let Some(first) = errors.into_iter().next() {
        return Err(CliError::Writer(first));
    }
    let graph_counts = GraphCounts::from_indexes(&crate::indexes::HotIndexes::load_from_cozo(
        &store_for_counts,
    )?);
    print_index_summary_parts(
        summary.files_scanned as u64,
        summary.files_indexed as u64,
        summary.nodes_created,
        summary.edges_created,
        Some(graph_counts),
        persistent.root_dir(),
    );
    Ok(ExitCode::SUCCESS)
}

fn maybe_prompt_mcp_install() {
    let candidates = crate::mcp_install::clients_needing_install();
    if !candidates.is_empty()
        && let Err(err) = crate::mcp_install::prompt_and_install(&candidates)
    {
        eprintln!("xgraph: MCP install skipped: {err}");
    }
}

fn cmd_mcp() -> Result<ExitCode, CliError> {
    let config = crate::mcp::McpConfig::with_router(Arc::new(GitProjectRouter));

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

struct GitProjectRouter;

impl crate::mcp::ProjectRouter for GitProjectRouter {
    fn route(
        &self,
        project_root: &str,
    ) -> Result<crate::mcp::DaemonEndpoint, crate::mcp::McpError> {
        let worktree = WorktreeRoot::discover(Path::new(project_root))
            .map_err(|err| crate::mcp::McpError::Unavailable(err.to_string()))?;
        let runtime = ensure_runtime_dir(worktree.as_path())
            .map_err(|err| crate::mcp::McpError::Unavailable(err.to_string()))?;
        Ok(crate::mcp::DaemonEndpoint {
            project_root: worktree.as_path().to_path_buf(),
            runtime_dir: runtime.as_path().to_path_buf(),
            daemon_launcher: Arc::new(SubprocessLauncher {
                worktree_root: worktree.as_path().to_path_buf(),
            }),
        })
    }
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
            let mut command = std::process::Command::new(exe_path);
            command
                .arg("daemon")
                .arg("start")
                .current_dir(&cwd)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null());

            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                // The daemon is intentionally longer-lived than the MCP
                // proxy. Put it in its own process group so cleanup for a
                // short-lived client process group cannot take it down.
                command.process_group(0);
            }

            let mut child = command.spawn().map_err(crate::mcp::McpError::Io)?;
            let pid = child.id();
            let _ = std::thread::Builder::new()
                .name("xgraph-daemon-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            Ok(crate::mcp::SpawnedDaemon::subprocess(pid))
        })
    }
}

fn cmd_daemon_start(project_root: Option<&Path>) -> Result<ExitCode, CliError> {
    let worktree = requested_worktree(project_root)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    persistent.ensure_created()?;
    let runtime = ensure_runtime_dir(worktree.as_path())?;
    let daemon_lock = acquire_daemon_lock(&runtime)?;
    let daemon_lock_file = daemon_lock.into_file();

    let store = open_store_with_lock_retry(
        &persistent.cozo_db_path(),
        std::time::Duration::from_secs(60),
    )?;
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
    let (maintenance_tx, maintenance_rx) = crate::owner::maintenance_channel();
    let maintenance_gate = Arc::new(parking_lot::RwLock::new(()));

    let summary = owner.index_all()?;
    print_index_summary_parts(
        summary.files_scanned as u64,
        summary.files_indexed as u64,
        summary.nodes_created,
        summary.edges_created,
        Some(GraphCounts::from_indexes(&indexes)),
        persistent.root_dir(),
    );
    println!(
        "opening daemon socket at {}",
        runtime.socket_path().display()
    );

    // Start the watcher and hand its batches to an OS thread that owns the
    // WorktreeOwner. The thread loops until batch_rx closes (when the watcher
    // handle is dropped at daemon shutdown).
    //
    // Watcher startup can fail on inotify-exhausted systems (e.g. when
    // `fs.inotify.max_user_watches` is at the Linux default 65k and a
    // VSCode/rust-analyzer instance has eaten most of them). When that
    // happens we serve queries against the indexed graph but skip
    // incremental updates rather than refusing to start at all — the
    // alternative is the MCP client hanging on startup with no useful
    // signal.
    let (watcher_handle, batch_rx) = match crate::watcher::Watcher::start(
        worktree.as_path(),
        watcher_matcher,
        crate::watcher::WatcherConfig::default(),
    ) {
        Ok(pair) => {
            let (h, rx) = pair;
            (Some(h), Some(rx))
        }
        Err(err) => {
            eprintln!(
                "xgraph: watcher failed to start: {err}\n\
                 xgraph: serving the existing graph; incremental updates disabled.\n\
                 xgraph: bump fs.inotify.max_user_watches (e.g. sysctl \
                 fs.inotify.max_user_watches=524288) and restart to re-enable."
            );
            (None, None)
        }
    };

    let maintenance_gate_for_thread = Arc::clone(&maintenance_gate);
    let watcher_thread = std::thread::Builder::new()
        .name("xgraph-watcher-handler".into())
        .spawn(move || {
            // Two channel-pair shapes depending on whether the watcher
            // started. With a watcher we poll both batches and
            // maintenance commands; without one we only poll
            // maintenance and exit when the maintenance channel
            // closes.
            match batch_rx {
                Some(batch_rx) => loop {
                    crossbeam_channel::select! {
                        recv(batch_rx) -> msg => match msg {
                            Ok(batch) => process_watcher_batch(&mut owner, batch),
                            Err(_) => break,
                        },
                        recv(maintenance_rx) -> msg => match msg {
                            Ok(command) => run_maintenance_command(
                                &mut owner,
                                &maintenance_gate_for_thread,
                                command,
                            ),
                            Err(_) => break,
                        },
                    }
                },
                None => {
                    while let Ok(command) = maintenance_rx.recv() {
                        run_maintenance_command(&mut owner, &maintenance_gate_for_thread, command);
                    }
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
    let worktree_root_for_lifecycle = worktree_root_for_handler.clone();
    let result: Result<(), DaemonError> = runtime_tokio.block_on(async move {
        let handler = Arc::new(WorktreeHandler::with_maintenance(
            indexes,
            status,
            worktree_root_for_handler,
            store_for_handler,
            maintenance_tx,
            maintenance_gate,
        ));
        let mut config = DaemonConfig::new(runtime.as_path().to_path_buf(), handler);
        config.lifecycle.worktree_root = Some(worktree_root_for_lifecycle);
        config.lifecycle.persistent_root = Some(persistent.root_dir().to_path_buf());
        let handle = crate::daemon::start_with_lock(config, daemon_lock_file).await?;
        let socket_path = handle.socket_path().to_path_buf();
        eprintln!("daemon listening on {}", socket_path.display());

        if let Err(err) = wait_for_shutdown(handle.shutdown_subscriber()).await {
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

async fn wait_for_shutdown(
    mut daemon_shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
        changed = daemon_shutdown.changed() => {
            let _ = changed;
        }
    }
    Ok(())
}

fn process_watcher_batch(owner: &mut WorktreeOwner, batch: crate::watcher::BatchedChanges) {
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
    if batch.ignore_file_changed
        && let Err(err) = owner.reconcile_after_ignore_change()
    {
        eprintln!("watcher: ignore-change reconciliation failed: {err}");
    }
}

fn run_maintenance_command(
    owner: &mut WorktreeOwner,
    maintenance_gate: &parking_lot::RwLock<()>,
    command: crate::owner::MaintenanceCommand,
) {
    let _guard = maintenance_gate.write();
    match command {
        crate::owner::MaintenanceCommand::Sync { reply } => {
            let progress = crate::progress::Progress::start();
            let result = owner.sync_all_with_progress(&progress);
            progress.stop();
            let _ = reply.send(result);
        }
        crate::owner::MaintenanceCommand::Reindex { reply } => {
            let progress = crate::progress::Progress::start();
            let result = owner.reindex_all_with_progress(&progress);
            progress.stop();
            let _ = reply.send(result);
        }
    }
}

/// Outcome of [`stop_daemon`] so callers can surface a useful message.
enum DaemonStopOutcome {
    /// No daemon was running for this worktree.
    NotRunning,
    /// Signal was delivered.
    Stopped { pid: i32, forced: bool },
    /// PID file existed but the process was already gone.
    AlreadyDead { pid: i32 },
}

/// Stop the daemon (if any) for the given worktree. When `force` is
/// true, sends SIGKILL and also removes the socket / pid / lock files
/// up front so a wedged daemon can be replaced. Returns once the
/// signal has been delivered; doesn't wait for the process to exit.
///
/// Used both by `xgraph daemon stop` and by `xgraph reindex` to clear
/// the way before opening the Cozo store for exclusive write access.
fn stop_daemon(worktree: &Path, force: bool) -> Result<DaemonStopOutcome, CliError> {
    let runtime = runtime_dir(worktree)?;
    let pid_path = runtime.pid_file_path();
    if !pid_path.exists() {
        if force {
            // Clean up any stale runtime files even when there's no
            // pid file — they may be left over from a kill -9'd daemon.
            let _ = fs::remove_file(runtime.socket_path());
            let _ = fs::remove_file(runtime.daemon_lock_path());
        }
        return Ok(DaemonStopOutcome::NotRunning);
    }
    let pid_text = fs::read_to_string(&pid_path).map_err(|source| CliError::Io {
        path: pid_path.clone(),
        source,
    })?;
    let pid: i32 = pid_text.trim().parse().unwrap_or(0);
    if pid <= 0 {
        let _ = fs::remove_file(&pid_path);
        if force {
            let _ = fs::remove_file(runtime.socket_path());
            let _ = fs::remove_file(runtime.daemon_lock_path());
        }
        return Ok(DaemonStopOutcome::AlreadyDead { pid });
    }
    let signal = if force { "-9" } else { "-15" };
    let status = std::process::Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status();
    match status {
        Ok(s) if s.success() => {
            if force {
                // Give the process a moment to actually die, then
                // clean up the runtime files so a fresh start isn't
                // confused by stale state.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = fs::remove_file(runtime.socket_path());
                let _ = fs::remove_file(&pid_path);
                let _ = fs::remove_file(runtime.daemon_lock_path());
            }
            Ok(DaemonStopOutcome::Stopped { pid, forced: force })
        }
        Ok(_) | Err(_) => {
            // The process was already gone. Tidy up the pid file (and
            // socket + lock if --force) so the next start doesn't
            // think a daemon is still around.
            let _ = fs::remove_file(&pid_path);
            if force {
                let _ = fs::remove_file(runtime.socket_path());
                let _ = fs::remove_file(runtime.daemon_lock_path());
            }
            Ok(DaemonStopOutcome::AlreadyDead { pid })
        }
    }
}

fn cmd_daemon_stop(project_root: Option<&Path>, force: bool) -> Result<ExitCode, CliError> {
    let worktree = requested_worktree(project_root)?;
    match stop_daemon(worktree.as_path(), force)? {
        DaemonStopOutcome::NotRunning => println!("no daemon running for this worktree"),
        DaemonStopOutcome::Stopped { pid, forced } => {
            let signal = if forced { "SIGKILL" } else { "SIGTERM" };
            println!("sent {signal} to daemon pid {pid}");
        }
        DaemonStopOutcome::AlreadyDead { pid } => {
            println!("daemon pid {pid} no longer running")
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(project_root: Option<&Path>) -> Result<ExitCode, CliError> {
    let worktree = requested_worktree(project_root)?;
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

fn cmd_sync(project_root: Option<&Path>) -> Result<ExitCode, CliError> {
    // Sync is idempotent: every file is re-hashed and the hash-skip cache
    // (introduced in Phase P3) keeps untouched files from being re-extracted.
    // Drift between disk and the active manifest is healed by the same
    // pipeline as init.
    let worktree = requested_worktree(project_root)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    ensure_daemon_running(&worktree)?;
    let Some(response) =
        send_daemon_request_if_reachable(&worktree, "sync", serde_json::json!({}))?
    else {
        eprintln!("xgraph: daemon unavailable after startup");
        return Ok(ExitCode::FAILURE);
    };
    if print_daemon_error_if_any(&response) {
        return Ok(ExitCode::FAILURE);
    }
    let result = response.get("result").unwrap_or(&response);
    print_daemon_index_summary(&worktree, result, persistent.root_dir())?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_reindex(project_root: Option<&Path>) -> Result<ExitCode, CliError> {
    // Reindex truncates the graph relations and runs a full fresh scan.
    let worktree = requested_worktree(project_root)?;
    let persistent = PersistentPaths::for_worktree(&worktree)?;
    ensure_daemon_running(&worktree)?;
    let Some(response) =
        send_daemon_request_if_reachable(&worktree, "reindex", serde_json::json!({}))?
    else {
        eprintln!("xgraph: daemon unavailable after startup");
        return Ok(ExitCode::FAILURE);
    };
    if print_daemon_error_if_any(&response) {
        return Ok(ExitCode::FAILURE);
    }
    let result = response.get("result").unwrap_or(&response);
    print_daemon_index_summary(&worktree, result, persistent.root_dir())?;
    Ok(ExitCode::SUCCESS)
}

/// Stop any daemon running for this worktree and wait for the process
/// to actually exit. Used by every command that needs exclusive write
/// access to the Cozo store (`init`, `sync` via `init_at`, `reindex`).
///
/// We poll `/proc/<pid>` rather than waiting for the socket file to
/// disappear. The socket goes away early in the daemon's shutdown
/// sequence (right after the accept loop breaks), but the RocksDB
/// LOCK isn't released until the *process* exits — that's when all
/// `CozoStore` clones (including those held by per-connection tokio
/// tasks and the watcher-owned `WorktreeOwner`) finally drop.
fn ensure_no_running_daemon(worktree: &Path) -> Result<(), CliError> {
    use std::time::{Duration, Instant};
    let pid = match stop_daemon(worktree, false)? {
        DaemonStopOutcome::NotRunning | DaemonStopOutcome::AlreadyDead { .. } => return Ok(()),
        DaemonStopOutcome::Stopped { pid, .. } => {
            eprintln!("xgraph: stopped daemon pid {pid} to take exclusive store lock");
            pid
        }
    };
    // First grace window: graceful shutdown.
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !process_alive(pid) {
        return Ok(());
    }
    // Process is still alive past SIGTERM grace — likely wedged in
    // tokio shutdown or holding the writer thread join. SIGKILL.
    eprintln!("xgraph: daemon pid {pid} didn't exit; sending SIGKILL");
    let _ = stop_daemon(worktree, true)?;
    // Second grace window: OS-forced exit. SIGKILL is immediate but
    // the kernel still needs a moment to reap the process and release
    // any held locks (RocksDB LOCK in particular).
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// True iff `/proc/<pid>` exists — i.e. the process is still alive.
/// Returns false for any non-positive pid so the wait loops fall
/// through without spinning.
fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

fn acquire_startup_lock_with_retry(
    runtime: &RuntimeDir,
    total_budget: std::time::Duration,
) -> Result<StartupLockGuard, CliError> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + total_budget;
    let mut delay = Duration::from_millis(50);
    let mut announced = false;
    loop {
        match acquire_startup_lock(runtime) {
            Ok(guard) => return Ok(guard),
            Err(RuntimeError::StartupLockHeld { .. }) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(RuntimeError::StartupLockHeld {
                        path: runtime.startup_lock_path(),
                    }
                    .into());
                }
                if !announced {
                    eprintln!(
                        "xgraph: startup lock is held by another process; waiting up to {}s",
                        total_budget.as_secs()
                    );
                    announced = true;
                }
                std::thread::sleep(delay.min(deadline.saturating_duration_since(now)));
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn open_store_with_lock_retry(
    path: &Path,
    total_budget: std::time::Duration,
) -> Result<CozoStore, CliError> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + total_budget;
    let mut delay = Duration::from_millis(50);
    let mut announced = false;
    loop {
        match CozoStore::open(path) {
            Ok(store) => return Ok(store),
            Err(err) if is_lock_contention(&err) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(err.into());
                }
                if !announced {
                    eprintln!(
                        "xgraph: cozo store is locked by another process; waiting up to {}s",
                        total_budget.as_secs()
                    );
                    announced = true;
                }
                std::thread::sleep(delay.min(deadline.saturating_duration_since(now)));
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn is_lock_contention(err: &crate::cozo::CozoError) -> bool {
    let s = err.to_string();
    s.contains("Resource temporarily unavailable")
        || (s.contains("lock file") && s.contains("LOCK"))
}

/// Send a JSON-RPC request to the worktree's daemon socket and pretty-
/// print the response payload. Used by `search` / `context` / `callers`
/// / `callees` / `impact` / `trace` / `files` / `find-symbol` CLI
/// subcommands.
///
/// The daemon is the source of truth — running a query via CLI is just a
/// thin client over the same socket the MCP transport uses. No language
/// extractors are loaded in this process; all the work happens daemon-side.
fn cmd_send_query(
    project_root: Option<&Path>,
    method: &str,
    params: serde_json::Value,
) -> Result<ExitCode, CliError> {
    let worktree = requested_worktree(project_root)?;
    ensure_daemon_running(&worktree)?;
    let parsed = match send_daemon_request_if_reachable(&worktree, method, params.clone())? {
        Some(parsed) => parsed,
        None => {
            ensure_daemon_running(&worktree)?;
            let Some(parsed) = send_daemon_request_if_reachable(&worktree, method, params)? else {
                let runtime = runtime_dir(worktree.as_path())?;
                eprintln!(
                    "xgraph: daemon socket not found at {} after startup.",
                    runtime.socket_path().display()
                );
                return Ok(ExitCode::FAILURE);
            };
            parsed
        }
    };
    if print_daemon_error_if_any(&parsed) {
        return Ok(ExitCode::FAILURE);
    }
    let pretty = serde_json::to_string_pretty(&parsed.get("result").unwrap_or(&parsed))
        .unwrap_or_else(|_| parsed.to_string());
    println!("xgraph project: {}", worktree.as_path().display());
    println!();
    println!("{pretty}");
    if let Some(meta) = parsed.get("meta")
        && let Some(catching_up) = meta.get("catching_up").and_then(|v| v.as_bool())
        && catching_up
    {
        eprintln!("note: daemon is catching up — result may be stale");
    }
    Ok(ExitCode::SUCCESS)
}

fn ensure_daemon_running(worktree: &WorktreeRoot) -> Result<(), CliError> {
    use std::time::{Duration, Instant};

    let runtime = ensure_runtime_dir(worktree.as_path())?;
    if socket_connects(&runtime.socket_path()) {
        return Ok(());
    }
    let _startup_guard = acquire_startup_lock_with_retry(&runtime, Duration::from_secs(60))?;
    if socket_connects(&runtime.socket_path()) {
        return Ok(());
    }
    let _ = fs::remove_file(runtime.socket_path());
    let _ = fs::remove_file(runtime.pid_file_path());
    let child_pid = spawn_daemon_process(worktree)?;

    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if socket_connects(&runtime.socket_path()) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    terminate_spawned_daemon(child_pid, &runtime);
    Err(CliError::Mcp(crate::mcp::McpError::StartupTimeout))
}

fn socket_connects(socket_path: &Path) -> bool {
    socket_path.exists() && std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn spawn_daemon_process(worktree: &WorktreeRoot) -> Result<u32, CliError> {
    let exe_path = env::current_exe().map_err(|source| CliError::Io {
        path: PathBuf::from("<current executable>"),
        source,
    })?;
    let mut command = std::process::Command::new(exe_path);
    command
        .arg("--project-root")
        .arg(worktree.as_path())
        .arg("daemon")
        .arg("start")
        .current_dir(worktree.as_path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|source| CliError::Io {
        path: worktree.as_path().to_path_buf(),
        source,
    })?;
    let pid = child.id();
    let _ = std::thread::Builder::new()
        .name("xgraph-cli-daemon-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(pid)
}

fn terminate_spawned_daemon(pid: u32, runtime: &RuntimeDir) {
    let pid = match i32::try_from(pid) {
        Ok(pid) if pid > 0 => pid,
        _ => return,
    };
    let _ = std::process::Command::new("kill")
        .arg("-15")
        .arg(pid.to_string())
        .status();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while process_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if process_alive(pid) {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }
    let _ = fs::remove_file(runtime.socket_path());
    let _ = fs::remove_file(runtime.pid_file_path());
}

fn send_daemon_request_if_reachable(
    worktree: &WorktreeRoot,
    method: &str,
    params: serde_json::Value,
) -> Result<Option<serde_json::Value>, CliError> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let runtime = runtime_dir(worktree.as_path())?;
    let socket_path = runtime.socket_path();
    if !socket_path.exists() {
        return Ok(None);
    }

    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(None);
        }
        Err(source) => {
            return Err(CliError::Io {
                path: socket_path,
                source,
            });
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
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
    match serde_json::from_str(response.trim()) {
        Ok(v) => Ok(Some(v)),
        Err(err) => {
            eprintln!("xgraph: malformed daemon response: {err}");
            eprintln!("raw: {response}");
            Ok(Some(serde_json::json!({
                "error": { "message": "malformed daemon response" }
            })))
        }
    }
}

fn print_daemon_error_if_any(response: &serde_json::Value) -> bool {
    let Some(err) = response.get("error") else {
        return false;
    };
    eprintln!(
        "xgraph: {}",
        err.get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
    );
    true
}

fn print_daemon_index_summary(
    worktree: &WorktreeRoot,
    result: &serde_json::Value,
    dir: &Path,
) -> Result<(), CliError> {
    let graph = match GraphCounts::from_json(result) {
        Some(graph) => Some(graph),
        None => daemon_graph_counts(worktree)?,
    };
    print_index_summary(result, graph, dir);
    Ok(())
}

fn daemon_graph_counts(worktree: &WorktreeRoot) -> Result<Option<GraphCounts>, CliError> {
    let Some(response) =
        send_daemon_request_if_reachable(worktree, "status", serde_json::json!({}))?
    else {
        return Ok(None);
    };
    if response.get("error").is_some() {
        return Ok(None);
    }
    let result = response.get("result").unwrap_or(&response);
    Ok(GraphCounts::from_status_json(result))
}

#[derive(Debug, Clone, Copy)]
struct GraphCounts {
    files: usize,
    nodes: usize,
    symbols: usize,
    call_edges: usize,
}

impl GraphCounts {
    fn from_indexes(indexes: &crate::indexes::HotIndexes) -> Self {
        Self {
            files: indexes.file_count(),
            nodes: indexes.node_count(),
            symbols: indexes.symbol_count(),
            call_edges: indexes.call_edge_count(),
        }
    }

    fn from_json(value: &serde_json::Value) -> Option<Self> {
        let graph = value.get("graph")?;
        Self::from_graph_object(graph)
    }

    fn from_status_json(value: &serde_json::Value) -> Option<Self> {
        Self::from_graph_object(value)
    }

    fn from_graph_object(graph: &serde_json::Value) -> Option<Self> {
        Some(Self {
            files: graph.get("files")?.as_u64()?.try_into().ok()?,
            nodes: graph.get("nodes")?.as_u64()?.try_into().ok()?,
            symbols: graph.get("symbols")?.as_u64()?.try_into().ok()?,
            call_edges: graph.get("call_edges")?.as_u64()?.try_into().ok()?,
        })
    }
}

fn print_index_summary(result: &serde_json::Value, graph: Option<GraphCounts>, dir: &Path) {
    let scanned = result.get("files_scanned").and_then(|v| v.as_u64());
    let files = result.get("files_indexed").and_then(|v| v.as_u64());
    let nodes = result.get("nodes_created").and_then(|v| v.as_u64());
    let edges = result.get("edges_created").and_then(|v| v.as_u64());
    match (files, nodes, edges) {
        (Some(files), Some(nodes), Some(edges)) => {
            print_index_summary_parts(scanned.unwrap_or(files), files, nodes, edges, graph, dir);
        }
        _ => println!(
            "{}",
            serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
        ),
    }
}

fn print_index_summary_parts(
    scanned: u64,
    changed: u64,
    nodes: u64,
    edges: u64,
    graph: Option<GraphCounts>,
    dir: &Path,
) {
    if scanned > changed {
        print!("checked {scanned} files: indexed {changed} changed files");
    } else {
        print!("indexed {changed} files");
    }

    if changed > 0 || graph.is_none() {
        print!(": {nodes} nodes, {edges} edges");
    }

    if let Some(graph) = graph {
        print!(
            "; graph has {files} files, {nodes} nodes, {symbols} symbols, {call_edges} call edges",
            files = graph.files,
            nodes = graph.nodes,
            symbols = graph.symbols,
            call_edges = graph.call_edges,
        );
    }

    println!(" in {dir}", dir = dir.display());
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
                action: DaemonAction::Stop { force: false }
            }
        );
    }

    #[test]
    fn parses_daemon_stop_force_flag() {
        let cli = parse(["xgraph", "daemon", "stop", "--force"])
            .expect("daemon stop --force should parse");
        assert_eq!(
            cli.command,
            Command::Daemon {
                action: DaemonAction::Stop { force: true }
            }
        );
    }

    #[test]
    fn parses_status_command() {
        let cli = parse(["xgraph", "status"]).expect("status should parse");
        assert_eq!(cli.command, Command::Status);
    }

    #[test]
    fn status_command_does_not_start_daemon() {
        use std::process::Command as ProcessCommand;

        let tmp = tempfile::tempdir().expect("tempdir");
        let status = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .arg(tmp.path())
            .status()
            .expect("git init");
        assert!(status.success());

        let worktree = WorktreeRoot::discover(tmp.path()).expect("discover worktree");
        let runtime = ensure_runtime_dir(worktree.as_path()).expect("runtime dir");
        let _ = std::fs::remove_file(runtime.socket_path());
        let _ = std::fs::remove_file(runtime.pid_file_path());

        let exit = cmd_status(Some(tmp.path())).expect("status succeeds");

        assert_eq!(exit, ExitCode::SUCCESS);
        assert!(
            !runtime.socket_path().exists(),
            "status must not create a daemon socket"
        );
        assert!(
            !runtime.pid_file_path().exists(),
            "status must not start a daemon"
        );
    }

    #[test]
    fn parses_project_root_global_flag() {
        let cli = parse(["xgraph", "--project-root", "/tmp/project-a", "status"])
            .expect("project root flag should parse");
        assert_eq!(cli.project_root, Some(PathBuf::from("/tmp/project-a")));
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
        let cli = parse(["xgraph", "find-symbol", "User", "--kind", "class"]).expect("parses");
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
                limit: None,
                offset: 0,
            }
        );
    }

    #[test]
    fn parses_impact_command_with_max_depth() {
        let cli = parse([
            "xgraph",
            "impact",
            "h:42",
            "--max-depth",
            "5",
            "--limit",
            "10",
            "--offset",
            "2",
        ])
        .expect("parses");
        assert_eq!(
            cli.command,
            Command::Impact {
                node_id: "h:42".to_string(),
                max_depth: 5,
                limit: Some(10),
                offset: 2,
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
            Command::Context {
                name,
                related_limit,
                ..
            } => {
                assert_eq!(name, "User");
                assert_eq!(related_limit, 20);
            }
            other => panic!("expected Context, got {other:?}"),
        }
    }

    #[test]
    fn parses_files_command() {
        let cli = parse([
            "xgraph",
            "files",
            "--prefix",
            "app/Services",
            "--limit",
            "10",
            "--offset",
            "20",
        ])
        .expect("parses");
        assert_eq!(
            cli.command,
            Command::Files {
                prefix: Some("app/Services".to_string()),
                limit: Some(10),
                offset: 20,
            }
        );
    }

    #[test]
    fn daemon_request_helper_uses_worktree_socket() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::process::Command as ProcessCommand;

        let tmp = tempfile::tempdir().expect("tempdir");
        let status = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .arg(tmp.path())
            .status()
            .expect("git init");
        assert!(status.success());

        let worktree = WorktreeRoot::discover(tmp.path()).expect("discover worktree");
        let runtime = ensure_runtime_dir(worktree.as_path()).expect("runtime dir");
        let socket_path = runtime.socket_path();
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind fake daemon");

        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut request = String::new();
            conn.read_to_string(&mut request).expect("read request");
            tx.send(request).expect("send request");
            conn.write_all(
                br#"{"jsonrpc":"2.0","id":1,"result":{"files_indexed":1,"nodes_created":2,"edges_created":3}}"#,
            )
            .expect("write response");
            conn.write_all(b"\n").expect("write newline");
        });

        let response =
            send_daemon_request_if_reachable(&worktree, "reindex", serde_json::json!({}))
                .expect("request succeeds")
                .expect("socket reachable");
        let request = rx.recv().expect("request captured");
        server.join().expect("server joins");

        assert!(request.contains(r#""method":"reindex""#), "{request}");
        assert_eq!(response["result"]["files_indexed"], 1);
        assert_eq!(response["result"]["nodes_created"], 2);
        assert_eq!(response["result"]["edges_created"], 3);
        let _ = std::fs::remove_file(socket_path);
    }

    #[test]
    fn init_command_uses_live_daemon_as_sync() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::process::Command as ProcessCommand;

        let tmp = tempfile::tempdir().expect("tempdir");
        let status = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .arg(tmp.path())
            .status()
            .expect("git init");
        assert!(status.success());

        let worktree = WorktreeRoot::discover(tmp.path()).expect("discover worktree");
        let runtime = ensure_runtime_dir(worktree.as_path()).expect("runtime dir");
        let socket_path = runtime.socket_path();
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind fake daemon");

        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut request = String::new();
            conn.read_to_string(&mut request).expect("read request");
            tx.send(request).expect("send request");
            conn.write_all(
                br#"{"jsonrpc":"2.0","id":1,"result":{"files_indexed":5,"nodes_created":8,"edges_created":13}}"#,
            )
            .expect("write response");
            conn.write_all(b"\n").expect("write newline");
        });

        let exit = cmd_init_at(tmp.path()).expect("init command succeeds");
        let request = rx.recv().expect("request captured");
        server.join().expect("server joins");

        assert_eq!(exit, ExitCode::SUCCESS);
        assert!(request.contains(r#""method":"sync""#), "{request}");
        let _ = std::fs::remove_file(socket_path);
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
