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

/// `Route::get('/users', [UserController::class, 'index'])` in a PHP file
/// should emit a `routes_to` framework edge in Cozo, attributing the route
/// to its controller method via the Laravel resolver.
#[test]
fn laravel_route_emits_framework_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("routes")).unwrap();
    fs::write(
        tmp.path().join("routes").join("web.php"),
        "<?php\nuse App\\Http\\Controllers\\UserController;\nRoute::get('/users', [UserController::class, 'index']);\n",
    )
    .unwrap();

    init_at(tmp.path()).expect("init");

    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen");
    let rows = store
        .run_read(
            "?[source, target] := *edge[source, $kind, target, $prov, _conf]",
            [
                (
                    "kind".to_string(),
                    cozo::DataValue::from("routes_to".to_string()),
                ),
                (
                    "prov".to_string(),
                    cozo::DataValue::from("laravel_heuristic".to_string()),
                ),
            ]
            .into(),
        )
        .expect("read edges");
    let edges: Vec<(String, String)> = rows
        .rows
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let src = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            let dst = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            Some((src, dst))
        })
        .collect();
    assert!(
        edges
            .iter()
            .any(|(s, t)| s.contains("/users") && t.contains("UserController::index")),
        "expected a routes_to edge from /users to UserController::index, got {edges:?}",
    );
}

/// Deleting a file on disk and re-running init must remove its active rows
/// from Cozo. Proxies the watcher's process_delete path via cmd_reindex
/// (which truncates then re-indexes).
#[test]
fn reindex_drops_facts_for_deleted_files() {
    use std::env;
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("kept.py"), "def kept_fn():\n    return 1\n").unwrap();
    fs::write(tmp.path().join("gone.py"), "def gone_fn():\n    return 2\n").unwrap();

    init_at(tmp.path()).expect("first init");

    // Verify both symbols indexed.
    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    {
        let store = CozoStore::open(&cozo_path).expect("open store");
        let idx = HotIndexes::load_from_cozo(&store).expect("load");
        assert!(!idx.lookup_symbol_by_name("kept_fn").is_empty());
        assert!(!idx.lookup_symbol_by_name("gone_fn").is_empty());
    }

    // Delete one file, run reindex (which truncates first).
    fs::remove_file(tmp.path().join("gone.py")).unwrap();
    let original = env::current_dir().expect("cwd");
    env::set_current_dir(tmp.path()).unwrap();
    let _ = xgraph::cli::run(["xgraph", "reindex"].into_iter().map(String::from));
    env::set_current_dir(original).unwrap();

    let store = CozoStore::open(&cozo_path).expect("reopen");
    let idx = HotIndexes::load_from_cozo(&store).expect("load2");
    assert!(
        !idx.lookup_symbol_by_name("kept_fn").is_empty(),
        "kept_fn should still be indexed"
    );
    assert!(
        idx.lookup_symbol_by_name("gone_fn").is_empty(),
        "gone_fn should be absent after deletion + reindex"
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
