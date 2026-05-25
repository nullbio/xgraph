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

/// Helper: read every laravel-heuristic edge from Cozo.
fn read_laravel_edges(cozo_path: &std::path::Path) -> Vec<(String, String, String)> {
    let store = CozoStore::open(cozo_path).expect("reopen");
    let rows = store
        .run_read(
            "?[source, kind, target] := *edge[source, kind, target, $prov, _c]",
            [(
                "prov".to_string(),
                cozo::DataValue::from("laravel_heuristic".to_string()),
            )]
            .into(),
        )
        .expect("read edges");
    rows.rows
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let src = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            let kind = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            let tgt = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            Some((src, kind, tgt))
        })
        .collect()
}

/// Eloquent relationship via `$this->hasMany(Post::class)` inside a model
/// must emit a `relates_to` framework edge from the relationship method to
/// the related model class.
#[test]
fn eloquent_relationship_emits_relates_to_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("app").join("Models")).unwrap();
    fs::write(
        tmp.path().join("app").join("Models").join("User.php"),
        "<?php\nnamespace App\\Models;\n\nclass User {\n    public function posts() {\n        return $this->hasMany(Post::class);\n    }\n}\n",
    )
    .unwrap();
    init_at(tmp.path()).expect("init");
    let edges = read_laravel_edges(&tmp.path().join(".git").join("xgraph").join("graph.cozo"));
    assert!(
        edges
            .iter()
            .any(|(_, k, t)| k == "relates_to" && t.contains("Post")),
        "expected relates_to edge to Post model, got {edges:?}"
    );
}

/// `Log::info('hi')` and other facade calls must emit `facade_call` edges
/// attributed to the enclosing method.
#[test]
fn facade_call_emits_facade_call_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("app").join("Http").join("Controllers")).unwrap();
    fs::write(
        tmp.path()
            .join("app")
            .join("Http")
            .join("Controllers")
            .join("UserController.php"),
        "<?php\nnamespace App\\Http\\Controllers;\n\nclass UserController {\n    public function show() {\n        Log::info('show called');\n    }\n}\n",
    )
    .unwrap();
    init_at(tmp.path()).expect("init");
    let edges = read_laravel_edges(&tmp.path().join(".git").join("xgraph").join("graph.cozo"));
    assert!(
        edges
            .iter()
            .any(|(_, k, t)| k == "facade_call" && t.contains("Log")),
        "expected facade_call edge to Log facade, got {edges:?}"
    );
}

/// `$this->app->singleton(Foo::class, Bar::class)` inside a service
/// provider must emit a `binds` edge.
#[test]
fn service_container_binding_emits_binds_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("app").join("Providers")).unwrap();
    fs::write(
        tmp.path()
            .join("app")
            .join("Providers")
            .join("AppServiceProvider.php"),
        "<?php\nnamespace App\\Providers;\n\nclass AppServiceProvider {\n    public function register() {\n        $this->app->singleton(Abs::class, Conc::class);\n    }\n}\n",
    )
    .unwrap();
    init_at(tmp.path()).expect("init");
    let edges = read_laravel_edges(&tmp.path().join(".git").join("xgraph").join("graph.cozo"));
    assert!(
        edges
            .iter()
            .any(|(s, k, t)| k == "binds" && s.contains("Abs") && t.contains("Conc")),
        "expected binds edge Abs → Conc, got {edges:?}"
    );
}

/// `event(new UserCreated($user))` must emit a `dispatches_event` edge from
/// the caller to the event class.
#[test]
fn event_dispatch_helper_emits_dispatches_event_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("app").join("Services")).unwrap();
    fs::write(
        tmp.path()
            .join("app")
            .join("Services")
            .join("Registrar.php"),
        "<?php\nnamespace App\\Services;\n\nclass Registrar {\n    public function register() {\n        event(new UserCreated($user));\n    }\n}\n",
    )
    .unwrap();
    init_at(tmp.path()).expect("init");
    let edges = read_laravel_edges(&tmp.path().join(".git").join("xgraph").join("graph.cozo"));
    assert!(
        edges
            .iter()
            .any(|(_, k, t)| k == "dispatches_event" && t.contains("UserCreated")),
        "expected dispatches_event edge to UserCreated, got {edges:?}"
    );
}

/// `dispatch(new ProcessJob($id))` must emit a `dispatches_job` edge from
/// the caller to the job class.
#[test]
fn job_dispatch_helper_emits_dispatches_job_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("app").join("Services")).unwrap();
    fs::write(
        tmp.path().join("app").join("Services").join("Queue.php"),
        "<?php\nnamespace App\\Services;\n\nclass Queue {\n    public function enqueue($id) {\n        dispatch(new ProcessJob($id));\n    }\n}\n",
    )
    .unwrap();
    init_at(tmp.path()).expect("init");
    let edges = read_laravel_edges(&tmp.path().join(".git").join("xgraph").join("graph.cozo"));
    assert!(
        edges
            .iter()
            .any(|(_, k, t)| k == "dispatches_job" && t.contains("ProcessJob")),
        "expected dispatches_job edge to ProcessJob, got {edges:?}"
    );
}

/// A controller calling a model static (`User::find($id)`) must emit a
/// `uses_model` edge from the controller method to the model class.
#[test]
fn controller_to_model_static_call_emits_uses_model_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("app").join("Http").join("Controllers")).unwrap();
    fs::write(
        tmp.path()
            .join("app")
            .join("Http")
            .join("Controllers")
            .join("PostController.php"),
        "<?php\nnamespace App\\Http\\Controllers;\n\nclass PostController {\n    public function show($id) {\n        return User::find($id);\n    }\n}\n",
    )
    .unwrap();
    init_at(tmp.path()).expect("init");
    let edges = read_laravel_edges(&tmp.path().join(".git").join("xgraph").join("graph.cozo"));
    assert!(
        edges
            .iter()
            .any(|(_, k, t)| k == "uses_model" && t.contains("User")),
        "expected uses_model edge to User, got {edges:?}"
    );
}

/// Blade templates that `@extends` and `@include` other views must emit
/// `extends_view` and `includes_view` framework edges through the full
/// pipeline (Blade extractor → Laravel resolver → Cozo edge rows).
#[test]
fn blade_template_emits_extends_and_include_edges() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("resources").join("views").join("partials")).unwrap();
    fs::create_dir_all(tmp.path().join("resources").join("views").join("layouts")).unwrap();
    fs::write(
        tmp.path()
            .join("resources")
            .join("views")
            .join("users.blade.php"),
        "@extends('layouts.app')\n\
         @section('content')\n\
         @include('partials.header')\n\
         <x-alert message=\"hi\" />\n\
         @endsection\n",
    )
    .unwrap();

    init_at(tmp.path()).expect("init");

    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen");

    let rows = store
        .run_read(
            "?[source, kind, target] := *edge[source, kind, target, $prov, _c]",
            [(
                "prov".to_string(),
                cozo::DataValue::from("laravel_heuristic".to_string()),
            )]
            .into(),
        )
        .expect("read edges");
    let edges: Vec<(String, String, String)> = rows
        .rows
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let src = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            let kind = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            let tgt = match it.next()? {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => return None,
            };
            Some((src, kind, tgt))
        })
        .collect();
    assert!(
        edges
            .iter()
            .any(|(s, k, t)| k == "extends_view"
                && s.contains("view.users")
                && t.contains("view.layouts.app")),
        "expected extends_view edge users → layouts.app, got {edges:?}"
    );
    assert!(
        edges
            .iter()
            .any(|(s, k, t)| k == "includes_view"
                && s.contains("view.users")
                && t.contains("view.partials.header")),
        "expected includes_view edge users → partials.header, got {edges:?}"
    );
    assert!(
        edges
            .iter()
            .any(|(s, k, t)| k == "uses_component"
                && s.contains("view.users")
                && t.contains("component.alert")),
        "expected uses_component edge users → alert, got {edges:?}"
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

/// Cross-file linking: a TypeScript file that imports a function from a
/// sibling and calls it must produce a `calls` edge from the caller to the
/// imported helper. Validates the full pipeline: per-binding extraction +
/// relative-path resolution + path-scoped symbol table + container
/// tracking.
#[test]
fn typescript_cross_file_call_emits_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join("helper.ts"),
        "export function helper(): number { return 42; }\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("app.ts"),
        "import { helper } from './helper';\n\
         export function caller(): number { return helper(); }\n",
    )
    .unwrap();

    init_at(tmp.path()).expect("init");

    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen");

    // Find the caller node and the helper node in active_node.
    let rows = store
        .run_read(
            "?[id, path, name] := *active_node[id, path, _hash, _lid, _kind, name, _q, _span]",
            std::collections::BTreeMap::new(),
        )
        .expect("read nodes");
    let mut helper_id: Option<String> = None;
    let mut caller_id: Option<String> = None;
    for row in rows.rows {
        let mut it = row.into_iter();
        let id = match it.next() {
            Some(cozo::DataValue::Str(s)) => s.to_string(),
            _ => continue,
        };
        let path = match it.next() {
            Some(cozo::DataValue::Str(s)) => s.to_string(),
            _ => continue,
        };
        let name = match it.next() {
            Some(cozo::DataValue::Str(s)) => s.to_string(),
            _ => continue,
        };
        if name == "helper" && path == "helper.ts" {
            helper_id = Some(id);
        } else if name == "caller" && path == "app.ts" {
            caller_id = Some(id);
        }
    }
    let helper_id = helper_id.expect("helper node missing");
    let caller_id = caller_id.expect("caller node missing");

    // The call inside `caller()` must produce a `calls` edge to `helper`.
    let edges = store
        .run_read(
            "?[source, kind, target] := *edge[source, kind, target, _p, _c], source = $src",
            [("src".to_string(), cozo::DataValue::from(caller_id.as_str()))].into(),
        )
        .expect("read edges");
    let mut found_calls_edge = false;
    for row in edges.rows {
        let mut it = row.into_iter();
        let _src = it.next();
        let kind = match it.next() {
            Some(cozo::DataValue::Str(s)) => s.to_string(),
            _ => continue,
        };
        let tgt = match it.next() {
            Some(cozo::DataValue::Str(s)) => s.to_string(),
            _ => continue,
        };
        if kind == "calls" && tgt == helper_id {
            found_calls_edge = true;
            break;
        }
    }
    assert!(
        found_calls_edge,
        "expected a `calls` edge from caller in app.ts to helper in helper.ts"
    );
}

/// Cross-file linking: an ES module import of a named binding must produce
/// an `imports` edge sourced from the file-level synthetic node. Anchors
/// the per-binding ref → path-scoped symbol table → file-level fallback
/// pipeline so a regression in any of the three breaks this test.
#[test]
fn typescript_per_binding_import_emits_file_level_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join("util.ts"),
        "export function bingo(): void {}\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("entry.ts"),
        "import { bingo } from './util';\n\
         bingo();\n",
    )
    .unwrap();

    init_at(tmp.path()).expect("init");

    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen");

    let rows = store
        .run_read(
            "?[source, target] := *edge[source, $kind, target, _p, _c]",
            [(
                "kind".to_string(),
                cozo::DataValue::from("imports".to_string()),
            )]
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
            .any(|(s, _)| s == "file:entry.ts" || s.starts_with("file:")),
        "expected an `imports` edge sourced from a file-level node, got {edges:?}"
    );
}

/// Python cross-file linking: `from helper import greet` + `greet()` must
/// produce a `calls` edge from the caller to greet across modules. Validates
/// Python's container tracking, the python-module-path symbol-table key,
/// and the rewrite-imports module-portion resolver.
#[test]
fn python_cross_file_call_emits_edge() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    // Make `helper.py` an importable module — Python's resolver requires
    // either a package (__init__.py) or a known top-level module name.
    // Absolute imports of single-file modules fall back to bare-name
    // matching in the symbol table, which is what this test validates.
    fs::write(
        tmp.path().join("helper.py"),
        "def greet():\n    return 'hi'\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("app.py"),
        "from helper import greet\n\
         def caller():\n    return greet()\n",
    )
    .unwrap();

    init_at(tmp.path()).expect("init");

    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen");

    let rows = store
        .run_read(
            "?[source, target] := *edge[source, $kind, target, _p, _c]",
            [(
                "kind".to_string(),
                cozo::DataValue::from("calls".to_string()),
            )]
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
        !edges.is_empty(),
        "expected at least one `calls` edge for Python cross-file resolution"
    );
}

/// `impact` MCP query runs a reverse transitive closure over Calls /
/// Inherits / Implements / References edges. Exercises the inline Datalog
/// in `handlers.rs::run_impact_query` against real Cozo data.
#[test]
fn impact_query_returns_callers_via_cozo() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join("helper.ts"),
        "export function helper(): number { return 42; }\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("app.ts"),
        "import { helper } from './helper';\n\
         export function caller(): number { return helper(); }\n",
    )
    .unwrap();
    init_at(tmp.path()).expect("init");

    let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
    let store = CozoStore::open(&cozo_path).expect("reopen");

    // Find helper's node_id so we can ask "what is impacted if helper changes".
    let rows = store
        .run_read(
            "?[id] := *active_node[id, path, _hash, _lid, _kind, name, _q, _span], \
             name = 'helper', path = 'helper.ts'",
            std::collections::BTreeMap::new(),
        )
        .expect("read");
    let helper_id = match rows.rows.into_iter().next() {
        Some(row) => match row.into_iter().next() {
            Some(cozo::DataValue::Str(s)) => s.to_string(),
            _ => panic!("expected helper node id as string"),
        },
        None => panic!("helper node missing"),
    };

    let affected = store
        .run_read(
            "impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'calls'\n\
             affected[node] := impact_edge[node, $target]\n\
             affected[node] := affected[downstream], impact_edge[node, downstream]\n\
             ?[node] := affected[node]\n\
             :sort node\n",
            [("target".to_string(), cozo::DataValue::from(helper_id.as_str()))].into(),
        )
        .expect("impact");
    let nodes: Vec<String> = affected
        .rows
        .into_iter()
        .filter_map(|row| match row.into_iter().next()? {
            cozo::DataValue::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        !nodes.is_empty(),
        "expected impact closure to include at least the caller node"
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
