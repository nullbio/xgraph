//! Command-line interface for the `xgraph` binary.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use crate::VERSION;
use crate::cozo::{
    ContentHash as CozoContentHash, CozoStore, FileUpdate, FileUpdateMetadata, WriterQueue,
};
use crate::git::{GitDiscoveryError, WorktreeRoot};
use crate::ignore::{IgnoreError, IgnoreMatcher};
use crate::language::{LanguageId, LanguageRegistry};
use crate::runtime::{RuntimeError, runtime_dir};
use crate::scanner::{ScanError, scan};
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
        }
    }
}

impl From<RuntimeError> for CliError {
    fn from(err: RuntimeError) -> Self {
        CliError::Runtime(err)
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

const PARSER_VERSION: u32 = 1;

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
    let scanned = scan(worktree.as_path(), &matcher)?;
    let registry = LanguageRegistry::with_all();

    let mut handle = WriterQueue::start(store)?;
    let mut indexed = 0usize;
    for file in scanned {
        let language = match file.language {
            Some(lang) => lang,
            None => continue,
        };
        let bytes = fs::read(&file.path).map_err(|source| CliError::Io {
            path: file.path.clone(),
            source,
        })?;
        let path_for_extract = file
            .path
            .strip_prefix(worktree.as_path())
            .unwrap_or(&file.path);
        let Some(extracted) = registry.extract_file(path_for_extract, &bytes) else {
            continue;
        };
        let metadata = FileUpdateMetadata {
            content_hash: cozo_content_hash(file.content_hash.as_bytes()),
            language: language_label(language).to_owned(),
            parser_version: PARSER_VERSION,
            mtime: mtime_seconds(file.mtime),
            size: file.size,
            generation: 1,
        };
        let update = FileUpdate::from_extracted(&extracted, metadata);
        handle.submit(update)?;
        indexed += 1;
    }
    handle.shutdown();

    let writer_errors = handle.take_errors();
    if !writer_errors.is_empty() {
        return Err(CliError::Writer(
            writer_errors.into_iter().next().expect("non-empty"),
        ));
    }

    println!(
        "indexed {indexed} files into {}",
        persistent.root_dir().display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cozo_content_hash(hash: &[u8; 32]) -> CozoContentHash {
    CozoContentHash::from_bytes(*hash)
}

fn mtime_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_secs() as i64,
        Err(err) => -(err.duration().as_secs() as i64),
    }
}

fn language_label(id: crate::scanner::DetectedLanguage) -> &'static str {
    use crate::scanner::DetectedLanguage::*;
    match id {
        Php => "php",
        Blade => "blade",
        JavaScript => "javascript",
        TypeScript => "typescript",
        Tsx => "tsx",
        Python => "python",
    }
}

#[allow(dead_code)]
fn _language_id_from_detected(id: crate::scanner::DetectedLanguage) -> LanguageId {
    use crate::scanner::DetectedLanguage::*;
    match id {
        Php => LanguageId::Php,
        Blade => LanguageId::Blade,
        JavaScript => LanguageId::JavaScript,
        TypeScript => LanguageId::TypeScript,
        Tsx => LanguageId::Tsx,
        Python => LanguageId::Python,
    }
}

fn cmd_mcp() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph mcp: requires full daemon spawn + proxy wiring; tracked for follow-up")
}

fn cmd_daemon_start() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph daemon start: requires owner+listener wiring; tracked for follow-up")
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
    let socket_present = runtime.socket_path().exists();
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
    println!(
        "daemon socket: {}",
        if socket_present { "present" } else { "absent" }
    );
    println!(
        "daemon pid:    {}",
        if pid_present { "present" } else { "absent" }
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_sync() -> Result<ExitCode, CliError> {
    // Until the daemon protocol exposes a sync RPC, sync is equivalent to re-running init:
    // walk the worktree, hash, extract, submit. The CozoStore is idempotent on file
    // replacement so this converges any drift between graph and disk.
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
    init_at(&cwd)
}

fn cmd_reindex() -> Result<ExitCode, CliError> {
    // Reindex from a clean slate. The integration phase will add a destructive
    // "drop content tables" step before this; for now reindex is sync.
    let cwd = env::current_dir().map_err(CliError::Cwd)?;
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
