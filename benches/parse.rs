//! Per-language full-file parse + extract benchmarks.
//!
//! Phase 11 of `IMPLEMENTATION_GUIDE.md` asks whether incremental parsing
//! (`old_tree.edit` + `parser.parse(new, Some(&old))`) is worth the memory
//! retention cost. These benchmarks establish the full-file parse baseline
//! per language on synthetic inputs at three sizes, so adding incremental
//! parsing later can be measured against them.
//!
//! Two operations per language:
//!   1. **parse_full**: how long does a cold thread-local parser take to
//!      build a fresh tree?
//!   2. **extract**: how long does the canonical extractor take to walk the
//!      tree?
//!
//! Cold-parser overhead is amortized after the first call thanks to the
//! `thread_local!` cache; the bench reports the steady-state cost.

use std::path::Path;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use xgraph::language::{LanguageId, LanguageRegistry};

fn gen_python_source(classes: usize, methods_per: usize) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..classes {
        out.push_str(&format!("class Class{i}:\n"));
        for m in 0..methods_per {
            out.push_str(&format!(
                "    def method_{m}(self, arg):\n        return arg + {m}\n"
            ));
        }
    }
    out.into_bytes()
}

fn gen_php_source(classes: usize, methods_per: usize) -> Vec<u8> {
    let mut out = String::from("<?php\nnamespace App;\n\n");
    for i in 0..classes {
        out.push_str(&format!("class Class{i} {{\n"));
        for m in 0..methods_per {
            out.push_str(&format!(
                "    public function method{m}(int $arg): int {{ return $arg + {m}; }}\n"
            ));
        }
        out.push_str("}\n\n");
    }
    out.into_bytes()
}

fn gen_ts_source(classes: usize, methods_per: usize) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..classes {
        out.push_str(&format!("export class Class{i} {{\n"));
        for m in 0..methods_per {
            out.push_str(&format!(
                "  method_{m}(arg: number): number {{ return arg + {m}; }}\n"
            ));
        }
        out.push_str("}\n");
    }
    out.into_bytes()
}

fn gen_go_source(types: usize, methods_per: usize) -> Vec<u8> {
    let mut out = String::from("package bench\n\n");
    for i in 0..types {
        out.push_str(&format!("type Type{i} struct {{}}\n"));
        out.push_str(&format!(
            "func NewType{i}() *Type{i} {{ return &Type{i}{{}} }}\n"
        ));
        for m in 0..methods_per {
            out.push_str(&format!(
                "func (t *Type{i}) Method{m}() {{ NewType{i}() }}\n"
            ));
        }
    }
    out.into_bytes()
}

fn gen_rust_source(types: usize, methods_per: usize) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..types {
        out.push_str(&format!("pub struct Type{i};\n"));
        out.push_str(&format!("pub fn new_type_{i}() -> Type{i} {{ Type{i} }}\n"));
        out.push_str(&format!("impl Type{i} {{\n"));
        for m in 0..methods_per {
            out.push_str(&format!(
                "    pub fn method_{m}(&self) {{ new_type_{i}(); }}\n"
            ));
        }
        out.push_str("}\n");
    }
    out.into_bytes()
}

fn bench_extract<F>(c: &mut Criterion, label: &str, id: LanguageId, path: &str, mut make_src: F)
where
    F: FnMut(usize, usize) -> Vec<u8>,
{
    let registry = LanguageRegistry::with_all();
    let plugin = registry.get(id).expect("plugin registered");
    let mut group = c.benchmark_group(label);
    for &(classes, methods) in &[(10usize, 5usize), (50, 5), (200, 10)] {
        let source = make_src(classes, methods);
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_function(format!("classes={classes}_methods={methods}"), |b| {
            b.iter(|| {
                let _ = plugin.extract(&source, Path::new(path));
            });
        });
    }
    group.finish();
}

fn bench_extract_python(c: &mut Criterion) {
    bench_extract(
        c,
        "extract_python",
        LanguageId::Python,
        "bench.py",
        gen_python_source,
    );
}

fn bench_extract_php(c: &mut Criterion) {
    bench_extract(
        c,
        "extract_php",
        LanguageId::Php,
        "bench.php",
        gen_php_source,
    );
}

fn bench_extract_typescript(c: &mut Criterion) {
    bench_extract(
        c,
        "extract_typescript",
        LanguageId::TypeScript,
        "bench.ts",
        gen_ts_source,
    );
}

fn bench_extract_go(c: &mut Criterion) {
    bench_extract(c, "extract_go", LanguageId::Go, "bench.go", gen_go_source);
}

fn bench_extract_rust(c: &mut Criterion) {
    bench_extract(
        c,
        "extract_rust",
        LanguageId::Rust,
        "bench.rs",
        gen_rust_source,
    );
}

criterion_group!(
    benches,
    bench_extract_python,
    bench_extract_php,
    bench_extract_typescript,
    bench_extract_go,
    bench_extract_rust,
);
criterion_main!(benches);
