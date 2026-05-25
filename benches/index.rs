//! Criterion benchmark scaffold for xgraph's core hot paths.
//!
//! These benchmarks are intentionally small — they exist to lock in the
//! Phase P1–P5 perf improvements and to give regressions somewhere to
//! surface. Add larger fixture-based benches as they become available.

use std::path::Path;
use std::process::Command;

use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

fn init_git_repo(root: &Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .arg(root)
        .status()
        .expect("git init");
    assert!(status.success());
}

fn bench_init_small_python(c: &mut Criterion) {
    c.bench_function("init_small_python_project", |b| {
        b.iter_with_setup(
            || {
                let tmp = TempDir::new().expect("tempdir");
                init_git_repo(tmp.path());
                for i in 0..10 {
                    let path = tmp.path().join(format!("module_{i}.py"));
                    std::fs::write(
                        &path,
                        format!("def func_{i}():\n    return {i}\n\nclass Class{i}:\n    pass\n"),
                    )
                    .unwrap();
                }
                tmp
            },
            |tmp| {
                let _ = xgraph::cli::init_at(tmp.path()).expect("init");
            },
        );
    });
}

fn bench_hash_skip_idempotent(c: &mut Criterion) {
    c.bench_function("hash_skip_second_init", |b| {
        b.iter_with_setup(
            || {
                let tmp = TempDir::new().expect("tempdir");
                init_git_repo(tmp.path());
                std::fs::write(tmp.path().join("a.py"), "def helper():\n    return 1\n").unwrap();
                let _ = xgraph::cli::init_at(tmp.path()).expect("priming init");
                tmp
            },
            |tmp| {
                // Second init must short-circuit via the hash-skip cache.
                let _ = xgraph::cli::init_at(tmp.path()).expect("second init");
            },
        );
    });
}

criterion_group!(benches, bench_init_small_python, bench_hash_skip_idempotent);
criterion_main!(benches);
