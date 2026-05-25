use std::fs;
use std::process::Command;

use tempfile::TempDir;
use xgraph::cli::init_at;
use xgraph::cozo::CozoStore;
use xgraph::indexes::HotIndexes;

fn init_git_repo(root: &std::path::Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .arg(root)
        .status()
        .expect("git init");
    assert!(status.success());
}

#[test]
fn init_indexes_a_minimal_git_repo() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join("hello.py"),
        "def greet():\n    return 'hi'\n",
    )
    .expect("write fixture");

    let result = init_at(tmp.path()).expect("init runs to completion");
    assert_eq!(result, std::process::ExitCode::SUCCESS);

    let xgraph_dir = tmp.path().join(".git").join("xgraph");
    assert!(
        xgraph_dir.exists(),
        "expected persistent dir at {}",
        xgraph_dir.display()
    );
    assert!(
        xgraph_dir.join("graph.cozo").exists() || xgraph_dir.join("graph.cozo.db").exists(),
        "expected Cozo DB inside {}",
        xgraph_dir.display()
    );
}

/// Crash-recovery proxy: simulate a daemon restart by re-opening the Cozo
/// store after `init_at` and rebuilding `HotIndexes::load_from_cozo`. The
/// daemon's actual startup runs the same calls, so this guards the recovery
/// path against regression without spawning a process.
#[test]
fn cozo_restart_repopulates_hot_indexes() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join("module.py"),
        "class User:\n    def greet(self):\n        return self.name\n",
    )
    .expect("write fixture");

    init_at(tmp.path()).expect("first init");

    // Open the same Cozo store fresh, the way `xgraph daemon start` would.
    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen store");
    let indexes = HotIndexes::load_from_cozo(&store).expect("load hot indexes");

    // We expect at least the `User` class symbol to be present.
    let hits = indexes.lookup_symbol_by_name("User");
    assert!(
        !hits.is_empty(),
        "expected 'User' symbol to be populated from Cozo after a fresh open"
    );
}

/// `init_at` is idempotent thanks to the hash-skip cache. Re-running it on
/// an unchanged worktree must succeed and must not corrupt the prior facts.
#[test]
fn init_is_idempotent_on_unchanged_worktree() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("a.py"), "def helper():\n    return 7\n").expect("write fixture");

    init_at(tmp.path()).expect("first init");
    // The second call must succeed without observing duplicate rows or errors.
    init_at(tmp.path()).expect("second init (no-op via hash skip)");

    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen");
    let indexes = HotIndexes::load_from_cozo(&store).expect("load");
    let hits = indexes.lookup_symbol_by_name("helper");
    assert_eq!(hits.len(), 1, "expected exactly one 'helper' symbol");
}
