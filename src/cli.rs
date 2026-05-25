//! Command-line interface for the `xgraph` binary.
//!
//! Parses arguments with `clap` and dispatches to per-command handlers.
//! The handlers are intentionally unimplemented in this unit; integration
//! phase will wire them to the discovery, daemon, scanner, and MCP modules.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::VERSION;

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
pub enum CliError {}

impl std::fmt::Display for CliError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

impl std::error::Error for CliError {}

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
    unimplemented!(
        "xgraph init: integration phase will wire Git discovery + Cozo schema + initial scan"
    )
}

fn cmd_mcp() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph mcp: integration phase will wire daemon launcher + proxy")
}

fn cmd_daemon_start() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph daemon start: integration phase will wire daemon launcher")
}

fn cmd_daemon_stop() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph daemon stop: integration phase will wire daemon shutdown")
}

fn cmd_status() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph status: integration phase will report daemon state and freshness")
}

fn cmd_sync() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph sync: integration phase will wire manifest reconciliation")
}

fn cmd_reindex() -> Result<ExitCode, CliError> {
    unimplemented!("xgraph reindex: integration phase will wire full rebuild")
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
