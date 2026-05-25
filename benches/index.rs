//! Initial-index and hot-query benchmarks.
//!
//! Covers Phase 19 metric tracking. Each benchmark generates a synthetic
//! fixture so the result is reproducible across machines / CI without
//! checking in real-world corpora.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tempfile::TempDir;

use xgraph::cli::init_at;
use xgraph::cozo::CozoStore;
use xgraph::indexes::{HotIndexes, NodeId, NodeRecord, SearchMode, SearchQuery, SymbolKey};

fn init_git_repo(root: &Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .arg(root)
        .status()
        .expect("git init");
    assert!(status.success());
}

/// Write `n` Python modules each containing one class with `methods_per`
/// methods. Returns the worktree root.
fn make_python_project(root: &Path, files: usize, methods_per: usize) {
    init_git_repo(root);
    for i in 0..files {
        let mut body = format!("class Class{i}:\n");
        for m in 0..methods_per {
            body.push_str(&format!("    def method_{m}(self):\n        return {m}\n"));
        }
        std::fs::write(root.join(format!("module_{i}.py")), body).unwrap();
    }
}

/// Same shape in PHP. Used to compare per-language parse + extract cost.
fn make_php_project(root: &Path, files: usize, methods_per: usize) {
    init_git_repo(root);
    for i in 0..files {
        let mut body = format!("<?php\nnamespace App;\n\nclass Class{i} {{\n");
        for m in 0..methods_per {
            body.push_str(&format!(
                "    public function method{m}(): int {{ return {m}; }}\n"
            ));
        }
        body.push_str("}\n");
        std::fs::write(root.join(format!("Module{i}.php")), body).unwrap();
    }
}

/// Cold `init_at` over a synthetic Python project. Throughput is set per
/// file so Criterion reports a per-file figure.
fn bench_init_python_cold(c: &mut Criterion) {
    let mut group = c.benchmark_group("init_python_cold");
    for &(files, methods) in &[(10usize, 5usize), (100, 5), (500, 10)] {
        group.throughput(Throughput::Elements(files as u64));
        group.bench_function(format!("files={files}_methods={methods}"), |b| {
            b.iter_with_setup(
                || {
                    let tmp = TempDir::new().unwrap();
                    make_python_project(tmp.path(), files, methods);
                    tmp
                },
                |tmp| {
                    let _ = init_at(tmp.path()).expect("init");
                },
            );
        });
    }
    group.finish();
}

fn bench_init_php_cold(c: &mut Criterion) {
    let mut group = c.benchmark_group("init_php_cold");
    for &(files, methods) in &[(10usize, 5usize), (100, 5), (500, 10)] {
        group.throughput(Throughput::Elements(files as u64));
        group.bench_function(format!("files={files}_methods={methods}"), |b| {
            b.iter_with_setup(
                || {
                    let tmp = TempDir::new().unwrap();
                    make_php_project(tmp.path(), files, methods);
                    tmp
                },
                |tmp| {
                    let _ = init_at(tmp.path()).expect("init");
                },
            );
        });
    }
    group.finish();
}

/// Re-running `init_at` on unchanged content should short-circuit via the
/// content-hash skip cache (Phase P3). Measures the skip-only path so any
/// regression in `CozoStore::active_file_hash` surfaces.
fn bench_hash_skip_second_init(c: &mut Criterion) {
    c.bench_function("hash_skip_python_100_files", |b| {
        b.iter_with_setup(
            || {
                let tmp = TempDir::new().unwrap();
                make_python_project(tmp.path(), 100, 5);
                init_at(tmp.path()).expect("priming");
                tmp
            },
            |tmp| {
                let _ = init_at(tmp.path()).expect("second init");
            },
        );
    });
}

/// `HotIndexes::load_from_cozo` runs on every daemon restart. The bench
/// covers a 200-file project pre-populated via `init_at`.
fn bench_hot_indexes_load(c: &mut Criterion) {
    c.bench_function("hot_indexes_load_python_200_files", |b| {
        b.iter_with_setup(
            || {
                let tmp = TempDir::new().unwrap();
                make_python_project(tmp.path(), 200, 5);
                init_at(tmp.path()).expect("priming");
                let cozo_path = tmp.path().join(".git").join("xgraph").join("graph.cozo");
                (tmp, cozo_path)
            },
            |(_tmp, cozo_path)| {
                let store = CozoStore::open(&cozo_path).expect("open");
                let _ = HotIndexes::load_from_cozo(&store).expect("load");
            },
        );
    });
}

/// `find_symbol` against a populated HotIndexes. Establishes the hot-query
/// latency baseline; should be sub-microsecond per lookup.
fn bench_hot_query_find_symbol(c: &mut Criterion) {
    let indexes = Arc::new(HotIndexes::new());
    for i in 0..5_000 {
        let id = NodeId::from(format!("h:{i}"));
        indexes.insert_node(NodeRecord {
            id: id.clone(),
            path: std::path::PathBuf::from(format!("mod_{}.rs", i / 100)),
            kind: "function".to_string(),
            name: format!("fn_{i}"),
            qname: format!("crate::mod_{}::fn_{}", i / 100, i),
        });
        indexes.register_symbol(
            SymbolKey {
                name: format!("fn_{i}"),
                kind: "function".to_string(),
            },
            id,
        );
    }
    c.bench_function("hot_query_find_symbol_exact", |b| {
        let needle = "fn_2500".to_string();
        b.iter(|| {
            let _ = indexes.lookup_symbol(&SymbolKey {
                name: needle.clone(),
                kind: "function".to_string(),
            });
        });
    });
    c.bench_function("hot_query_find_symbol_by_name_only", |b| {
        let needle = "fn_2500".to_string();
        b.iter(|| {
            let _ = indexes.lookup_symbol_by_name(&needle);
        });
    });
}

/// Search benchmarks at 10k and 50k symbols across exact / prefix /
/// contains modes. The exact path goes through the primary hash map and
/// should be sub-microsecond; prefix and contains scan the
/// `symbols_by_name` DashMap and depend on N_unique_names.
fn bench_hot_search(c: &mut Criterion) {
    for &n in &[10_000usize, 50_000] {
        let indexes = Arc::new(HotIndexes::new());
        for i in 0..n {
            let id = NodeId::from(format!("h:{i}"));
            indexes.insert_node(NodeRecord {
                id: id.clone(),
                path: std::path::PathBuf::from(format!("mod_{}.rs", i / 100)),
                kind: "function".to_string(),
                name: format!("fn_{i}"),
                qname: format!("crate::mod_{}::fn_{}", i / 100, i),
            });
            indexes.register_symbol(
                SymbolKey {
                    name: format!("fn_{i}"),
                    kind: "function".to_string(),
                },
                id,
            );
        }
        c.bench_function(format!("search_exact_n{n}").as_str(), |b| {
            b.iter(|| {
                let _ = indexes.search(&SearchQuery {
                    name: "fn_2500".to_string(),
                    mode: SearchMode::Exact,
                    kind: None,
                    path_prefix: None,
                    limit: 64,
                });
            });
        });
        c.bench_function(format!("search_prefix_n{n}").as_str(), |b| {
            b.iter(|| {
                let _ = indexes.search(&SearchQuery {
                    name: "fn_25".to_string(),
                    mode: SearchMode::Prefix,
                    kind: None,
                    path_prefix: None,
                    limit: 64,
                });
            });
        });
        c.bench_function(format!("search_contains_n{n}").as_str(), |b| {
            b.iter(|| {
                let _ = indexes.search(&SearchQuery {
                    name: "_2500".to_string(),
                    mode: SearchMode::Contains,
                    kind: None,
                    path_prefix: None,
                    limit: 64,
                });
            });
        });
    }
}

/// Per-phase breakdown of `index_all_with_progress`. Reports
/// scan/parse/resolve/store separately so the next optimization target
/// is obvious — e.g., if parse dominates, parallelize extraction; if
/// store dominates, batch Cozo transactions.
///
/// Each sample builds a fresh fixture, runs the full pipeline, and
/// returns only the requested phase's micros via `iter_custom`. The
/// downside is that all four phases re-run a fresh fixture build per
/// sample. That's acceptable here — we care about per-phase signal, not
/// wall-clock efficiency of the bench itself.
fn bench_index_phases(c: &mut Criterion) {
    use std::time::Duration;
    use xgraph::cozo::CozoStore;
    use xgraph::daemon_status::DaemonStatus;
    use xgraph::git::WorktreeRoot;
    use xgraph::ignore::IgnoreMatcher;
    use xgraph::indexes::HotIndexes;
    use xgraph::language::LanguageRegistry;
    use xgraph::owner::WorktreeOwner;
    use xgraph::storage::PersistentPaths;

    let fixtures = [(100usize, 5usize), (500, 10)];
    type PhaseExtract = fn(&xgraph::owner::PhaseTimings) -> u64;
    let phases: [(&str, PhaseExtract); 4] = [
        ("scan", |t| t.scan_us),
        ("parse", |t| t.parse_us),
        ("resolve", |t| t.resolve_us),
        ("store", |t| t.store_us),
    ];

    for &(files, methods) in &fixtures {
        for (phase, extract) in phases {
            let label = format!("phase_{phase}_files{files}_methods{methods}");
            c.bench_function(&label, |b| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let tmp = TempDir::new().unwrap();
                        make_python_project(tmp.path(), files, methods);

                        let worktree = WorktreeRoot::discover(tmp.path()).expect("git");
                        let persistent =
                            PersistentPaths::for_worktree(&worktree).expect("persistent");
                        persistent.ensure_created().expect("ensure");
                        let store =
                            CozoStore::open(&persistent.cozo_db_path()).expect("cozo");
                        let matcher = IgnoreMatcher::new(worktree.as_path()).expect("matcher");
                        let registry = LanguageRegistry::with_all();
                        let indexes = Arc::new(HotIndexes::new());
                        let status = Arc::new(DaemonStatus::new());
                        let mut owner = WorktreeOwner::new(
                            worktree.as_path().to_path_buf(),
                            matcher,
                            registry,
                            store,
                            indexes,
                            status,
                        )
                        .expect("owner");
                        let progress = xgraph::progress::Progress::start();
                        let summary = owner
                            .index_all_with_progress(&progress)
                            .expect("index_all");
                        progress.stop();
                        let _ = owner.shutdown();
                        total += Duration::from_micros(extract(&summary.timings));
                    }
                    total
                });
            });
        }
    }
}

criterion_group!(
    benches,
    bench_init_python_cold,
    bench_init_php_cold,
    bench_hash_skip_second_init,
    bench_hot_indexes_load,
    bench_hot_query_find_symbol,
    bench_hot_search,
    bench_index_phases,
);
criterion_main!(benches);
