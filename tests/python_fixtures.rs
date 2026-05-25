use std::fs;
use std::path::PathBuf;

use xgraph::languages::python::extract;

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
        .filter(|r| r.kind == "import")
        .map(|r| r.name.as_str())
        .collect();
    assert!(import_names.contains(&"os"));
    assert!(import_names.contains(&"List"));
    assert!(import_names.contains(&"foo"));
    assert!(import_names.contains(&"*"));

    let class = out
        .nodes
        .iter()
        .find(|n| n.name == "User")
        .expect("User class");
    assert_eq!(class.kind, "class");
    assert!(
        out.refs
            .iter()
            .any(|r| r.kind == "inheritance" && r.name == "BaseUser"),
        "expected BaseUser inheritance ref"
    );

    let fetch = out
        .nodes
        .iter()
        .find(|n| n.name == "fetch")
        .expect("fetch method");
    assert_eq!(fetch.kind, "method");
    assert_eq!(fetch.parent, Some(class.id));

    let index = out
        .nodes
        .iter()
        .find(|n| n.name == "index")
        .expect("index function");
    assert_eq!(index.kind, "function");
    assert!(
        out.refs.iter().any(|r| r.kind == "decorator"
            && r.name == "app.route('/')"
            && r.container == Some(index.id)),
        "expected decorator ref attached to index"
    );

    let chain = out
        .refs
        .iter()
        .find(|r| r.kind == "call" && r.name == "a.b.c")
        .expect("chained call");
    assert_eq!(chain.name, "a.b.c");

    let constants: Vec<&str> = out
        .nodes
        .iter()
        .filter(|n| n.kind == "constant")
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
    assert_eq!(good.kind, "function");
}
