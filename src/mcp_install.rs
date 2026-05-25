//! Interactive MCP-server registration for `xgraph init`.
//!
//! Detects whether Claude Code (`~/.claude.json`) or Codex
//! (`~/.codex/config.toml`) is installed, checks whether `xgraph` is
//! already registered as an MCP server in either, and offers — only
//! when stdin is a TTY — to install it for the user.
//!
//! Registration is global, not per-project: there is only one xgraph
//! binary, and the `xgraph mcp` command derives the worktree from the
//! current working directory at invocation time, so a single entry
//! works across every project the user opens.
//!
//! All edits preserve unrelated fields in each config file. Claude's
//! JSON is round-tripped through `serde_json::Value`; Codex's TOML uses
//! `toml_edit` so comments and section ordering survive.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("io error on {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("malformed JSON in {}: {source}", path.display())]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("malformed TOML in {}: {source}", path.display())]
    Toml {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
}

/// A detected MCP-capable client install on the user's system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Claude,
    Codex,
}

impl Client {
    pub fn label(&self) -> &'static str {
        match self {
            Client::Claude => "Claude Code",
            Client::Codex => "Codex",
        }
    }
}

/// Resolve the user's home directory. Returns `None` if neither `$HOME`
/// nor the platform-equivalent is set — in which case install detection
/// is a no-op.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn claude_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude.json"))
}

fn codex_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".codex").join("config.toml"))
}

/// Return every detected client that does NOT yet have xgraph
/// registered. Clients that aren't installed at all are silently
/// omitted; clients that already registered xgraph are also omitted.
pub fn clients_needing_install() -> Vec<Client> {
    let mut out = Vec::new();
    if let Some(p) = claude_config_path()
        && p.exists()
        && !claude_is_registered(&p).unwrap_or(false)
    {
        out.push(Client::Claude);
    }
    if let Some(p) = codex_config_path()
        && p.exists()
        && !codex_is_registered(&p).unwrap_or(false)
    {
        out.push(Client::Codex);
    }
    out
}

fn claude_is_registered(path: &Path) -> Result<bool, InstallError> {
    let text = fs::read_to_string(path).map_err(|source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| InstallError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(value
        .get("mcpServers")
        .and_then(|m| m.get("xgraph"))
        .is_some())
}

fn codex_is_registered(path: &Path) -> Result<bool, InstallError> {
    let text = fs::read_to_string(path).map_err(|source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let doc: toml_edit::DocumentMut = text.parse().map_err(|source| InstallError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(doc
        .get("mcp_servers")
        .and_then(|t| t.as_table())
        .and_then(|t| t.get("xgraph"))
        .is_some())
}

/// Prompt the user to install xgraph as an MCP server for each detected
/// client. Skips entirely when stdin is not a TTY (CI, redirected
/// input) so the init flow stays non-interactive in those cases.
///
/// The exe path baked into each config is the absolute path of the
/// currently running binary — so the config keeps working regardless
/// of the user's `$PATH`.
pub fn prompt_and_install(clients: &[Client]) -> Result<(), InstallError> {
    if clients.is_empty() {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        // Non-interactive: just surface the suggestion to stderr so the
        // user knows we noticed, but don't modify any config files.
        for client in clients {
            eprintln!(
                "[xgraph] {} detected but xgraph is not registered as an MCP server. \
                 Run `xgraph init` interactively to add it.",
                client.label()
            );
        }
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|source| InstallError::Io {
        path: PathBuf::from("<current exe>"),
        source,
    })?;
    // Canonicalize so symlinks (e.g. cargo bin → cargo target) resolve;
    // the registered config should survive PATH changes.
    let exe = exe.canonicalize().unwrap_or(exe);

    for &client in clients {
        let consent = ask_yes_no(&format!(
            "{} detected. Register xgraph as a global MCP server? [Y/n] ",
            client.label()
        ))?;
        if !consent {
            println!("  skipped {}", client.label());
            continue;
        }
        match client {
            Client::Claude => {
                let path = claude_config_path().expect("home_dir present");
                install_claude(&path, &exe)?;
            }
            Client::Codex => {
                let path = codex_config_path().expect("home_dir present");
                install_codex(&path, &exe)?;
            }
        }
        println!("  registered xgraph in {}", client.label());
    }
    Ok(())
}

fn ask_yes_no(prompt: &str) -> Result<bool, InstallError> {
    print!("{prompt}");
    io::stdout().flush().map_err(|source| InstallError::Io {
        path: PathBuf::from("<stdout>"),
        source,
    })?;
    let mut buf = String::new();
    io::stdin()
        .read_line(&mut buf)
        .map_err(|source| InstallError::Io {
            path: PathBuf::from("<stdin>"),
            source,
        })?;
    let trimmed = buf.trim().to_ascii_lowercase();
    Ok(trimmed.is_empty() || trimmed == "y" || trimmed == "yes")
}

fn install_claude(path: &Path, exe: &Path) -> Result<(), InstallError> {
    let text = fs::read_to_string(path).map_err(|source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| InstallError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    // Ensure mcpServers exists at the top level.
    let servers = value
        .as_object_mut()
        .expect("claude config root is an object")
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(Default::default()))
        .as_object_mut()
        .expect("mcpServers is an object");
    servers.insert(
        "xgraph".to_string(),
        serde_json::json!({
            "command": exe.to_string_lossy(),
            "args": ["mcp"],
        }),
    );
    let pretty = serde_json::to_string_pretty(&value).map_err(|source| InstallError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomically(path, pretty.as_bytes())
}

fn install_codex(path: &Path, exe: &Path) -> Result<(), InstallError> {
    let text = fs::read_to_string(path).map_err(|source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|source| InstallError::Toml {
        path: path.to_path_buf(),
        source,
    })?;

    // Build the nested table for the new server entry.
    let mut entry = toml_edit::Table::new();
    entry["command"] = toml_edit::value(exe.to_string_lossy().to_string());
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    entry["args"] = toml_edit::value(args);

    // Ensure `mcp_servers` exists as an implicit parent table so the
    // child is serialized as `[mcp_servers.xgraph]` (header form),
    // not `mcp_servers = { xgraph = {...} }` (inline form). Without
    // this, assigning `doc["mcp_servers"]["xgraph"] = ...` on an
    // absent parent creates an empty inline table and silently drops
    // the child.
    if !doc.contains_key("mcp_servers") {
        let mut parent = toml_edit::Table::new();
        parent.set_implicit(true);
        doc["mcp_servers"] = toml_edit::Item::Table(parent);
    }
    doc["mcp_servers"]["xgraph"] = toml_edit::Item::Table(entry);

    write_atomically(path, doc.to_string().as_bytes())
}

/// Replace `path`'s contents by writing to a tempfile in the same
/// directory and renaming. Rename-on-the-same-filesystem is atomic on
/// Linux, so the user's config is never left half-written even if the
/// process dies mid-write.
fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), InstallError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp_name = format!(
        ".xgraph-mcp-install.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp = parent.join(tmp_name);
    fs::write(&tmp, contents).map_err(|source| InstallError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn claude_registration_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("claude.json");
        fs::write(
            &path,
            r#"{"otherKey": 1, "mcpServers": {"existing": {"command": "/foo"}}}"#,
        )
        .unwrap();
        assert!(!claude_is_registered(&path).unwrap());
        install_claude(&path, Path::new("/usr/local/bin/xgraph")).unwrap();
        assert!(claude_is_registered(&path).unwrap());

        let after = fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v["otherKey"], 1, "unrelated keys must survive");
        assert_eq!(v["mcpServers"]["existing"]["command"], "/foo");
        assert_eq!(
            v["mcpServers"]["xgraph"]["command"],
            "/usr/local/bin/xgraph"
        );
        assert_eq!(v["mcpServers"]["xgraph"]["args"][0], "mcp");
    }

    #[test]
    fn claude_install_creates_mcpservers_when_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("claude.json");
        fs::write(&path, r#"{"otherKey": 1}"#).unwrap();
        install_claude(&path, Path::new("/usr/bin/xgraph")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["mcpServers"]["xgraph"].is_object());
    }

    #[test]
    fn codex_registration_round_trip_preserves_comments() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "\
# A comment we must not lose.
model = \"gpt-5\"

[mcp_servers.other]
url = \"https://example.com/mcp\"
";
        fs::write(&path, original).unwrap();
        assert!(!codex_is_registered(&path).unwrap());

        install_codex(&path, Path::new("/usr/local/bin/xgraph")).unwrap();
        assert!(codex_is_registered(&path).unwrap());

        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# A comment we must not lose."),
            "comments must be preserved; got:\n{after}"
        );
        assert!(after.contains("[mcp_servers.other]"));
        assert!(after.contains("[mcp_servers.xgraph]"));
        assert!(after.contains("/usr/local/bin/xgraph"));
    }

    #[test]
    fn codex_install_creates_mcp_servers_section_when_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "model = \"gpt-5\"\n").unwrap();
        install_codex(&path, Path::new("/usr/bin/xgraph")).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("[mcp_servers.xgraph]"));
        assert!(after.contains("/usr/bin/xgraph"));
        // The pre-existing key still exists.
        assert!(after.contains("model = \"gpt-5\""));
    }

    #[test]
    fn write_atomically_replaces_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("target.txt");
        fs::write(&path, "old").unwrap();
        write_atomically(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }
}
