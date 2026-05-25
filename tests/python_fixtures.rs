use std::fs;
use std::path::PathBuf;

use xgraph::languages::python::{NodeKind, RefKind, extract};

fn fixture(name: &str) -> Vec<u8> {
    let path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("python")
        .join(name);
    fs::read(&path).unwrap_or_else(|err| panic!("failed to read fixture {name}: {err}"))
}

#[test]
fn basic_fixture_produces_expected_facts() {
    let src = fixture("basic.py");
    let out = extract(&src);

    let import_names: Vec<&str> = out
        .refs
        .iter()
        .filter(|r| r.kind == RefKind::Import)
        .map(|r| r.name.as_str())
        .collect();
    assert!(import_names.contains(&"os"));
    assert!(import_names.contains(&"typing"));
    assert!(import_names.contains(&".helpers"));
    assert!(import_names.contains(&"..pkg"));

    let class = out
        .nodes
        .iter()
        .find(|n| n.name == "User")
        .expect("User class");
    assert_eq!(class.kind, NodeKind::Class);
    assert_eq!(class.bases, vec!["BaseUser".to_string()]);

    let fetch = out
        .nodes
        .iter()
        .find(|n| n.name == "fetch")
        .expect("fetch method");
    assert_eq!(fetch.kind, NodeKind::Method);
    assert!(fetch.is_async);

    let index = out
        .nodes
        .iter()
        .find(|n| n.name == "index")
        .expect("index function");
    assert_eq!(index.decorators, vec!["app.route('/')".to_string()]);

    let chain = out
        .refs
        .iter()
        .find(|r| {
            r.kind == RefKind::Call
                && r.items == vec!["a".to_string(), "b".to_string(), "c".to_string()]
        })
        .expect("chained call");
    assert_eq!(chain.name, "a.b.c");

    let constants: Vec<&str> = out
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Constant)
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(constants, vec!["MAX_RETRIES"]);

    assert!(out.diagnostics.is_empty(), "basic.py should parse cleanly");
}

#[test]
fn malformed_fixture_emits_diagnostic_with_partial_extraction() {
    let src = fixture("malformed.py");
    let out = extract(&src);

    assert!(!out.diagnostics.is_empty(), "expected diagnostic");
    let good = out
        .nodes
        .iter()
        .find(|n| n.name == "good")
        .expect("good function still captured");
    assert_eq!(good.kind, NodeKind::Function);
}
