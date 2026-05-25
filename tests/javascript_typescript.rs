use std::fs;
use std::path::PathBuf;

use xgraph::languages::javascript::{NodeKind, Ref, RefKind, extract as extract_js};
use xgraph::languages::typescript::{TsFlavor, TsNodeKind, TsRefKind, extract as extract_ts};

fn fixture(relative: &str) -> Vec<u8> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures");
    path.push(relative);
    fs::read(&path).unwrap_or_else(|err| panic!("failed to read fixture {relative}: {err}"))
}

fn import_names(refs: &[Ref]) -> Vec<String> {
    refs.iter()
        .filter(|r| matches!(r.kind, RefKind::ImportEsm | RefKind::ImportCjs))
        .map(|r| r.name.clone())
        .collect()
}

#[test]
fn javascript_imports_fixture_extracts_all_module_sources() {
    let source = fixture("javascript/imports.js");
    let file = extract_js(&source);
    let imports = import_names(&file.refs);
    assert!(imports.contains(&"mod-default".to_owned()));
    assert!(imports.contains(&"mod-named".to_owned()));
    assert!(imports.contains(&"mod-namespace".to_owned()));
    assert!(imports.contains(&"mod-side-effect".to_owned()));
    assert!(imports.contains(&"mod-required".to_owned()));
}

#[test]
fn javascript_imports_fixture_extracts_class_and_methods() {
    let source = fixture("javascript/imports.js");
    let file = extract_js(&source);
    let class_names: Vec<&str> = file
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Class))
        .map(|n| n.name.as_str())
        .collect();
    let method_names: Vec<&str> = file
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Method))
        .map(|n| n.name.as_str())
        .collect();
    let arrow_names: Vec<&str> = file
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::ArrowFunction))
        .map(|n| n.name.as_str())
        .collect();
    assert!(class_names.contains(&"Container"));
    assert!(method_names.contains(&"process"));
    assert!(arrow_names.contains(&"make"));
}

#[test]
fn javascript_cjs_fixture_captures_require_and_module_exports() {
    let source = fixture("javascript/cjs.cjs");
    let file = extract_js(&source);
    let cjs_imports: Vec<&str> = file
        .refs
        .iter()
        .filter(|r| matches!(r.kind, RefKind::ImportCjs))
        .map(|r| r.name.as_str())
        .collect();
    assert!(cjs_imports.contains(&"node:fs"));
    let cjs_exports: Vec<&str> = file
        .refs
        .iter()
        .filter(|r| matches!(r.kind, RefKind::ExportCjs))
        .map(|r| r.name.as_str())
        .collect();
    assert!(cjs_exports.contains(&"module.exports"));
    assert!(cjs_exports.contains(&"exports.fs"));
}

#[test]
fn javascript_mjs_fixture_extracts_esm() {
    let source = fixture("javascript/esm.mjs");
    let file = extract_js(&source);
    let imports = import_names(&file.refs);
    assert!(imports.contains(&"node:fs/promises".to_owned()));
    let esm_exports: Vec<&str> = file
        .refs
        .iter()
        .filter(|r| matches!(r.kind, RefKind::ExportEsm))
        .map(|r| r.name.as_str())
        .collect();
    assert!(esm_exports.contains(&"loadJson"));
    assert!(esm_exports.contains(&"default"));
}

#[test]
fn typescript_types_fixture_extracts_interfaces_and_aliases() {
    let source = fixture("typescript/types.ts");
    let file = extract_ts(TsFlavor::TypeScript, &source);
    let interfaces: Vec<&str> = file
        .type_nodes
        .iter()
        .filter(|n| matches!(n.kind, TsNodeKind::Interface))
        .map(|n| n.name.as_str())
        .collect();
    let aliases: Vec<&str> = file
        .type_nodes
        .iter()
        .filter(|n| matches!(n.kind, TsNodeKind::TypeAlias))
        .map(|n| n.name.as_str())
        .collect();
    assert!(interfaces.contains(&"Person"));
    assert!(aliases.contains(&"Maybe"));
}

#[test]
fn typescript_types_fixture_captures_generic_type_references() {
    let source = fixture("typescript/types.ts");
    let file = extract_ts(TsFlavor::TypeScript, &source);
    let names: Vec<&str> = file
        .type_refs
        .iter()
        .filter(|r| matches!(r.kind, TsRefKind::TypeReference))
        .map(|r| r.name.as_str())
        .collect();
    assert!(names.contains(&"Person"));
    assert!(names.contains(&"Service"));
    assert!(names.contains(&"Array"));
    assert!(names.contains(&"T"));
}

#[test]
fn tsx_component_fixture_captures_uppercase_components() {
    let source = fixture("typescript/component.tsx");
    let file = extract_ts(TsFlavor::Tsx, &source);
    let components: Vec<&str> = file
        .base
        .refs
        .iter()
        .filter(|r| matches!(r.kind, RefKind::JsxComponent))
        .map(|r| r.name.as_str())
        .collect();
    assert!(components.contains(&"Button"));
    assert!(!components.contains(&"div"));
}

#[test]
fn tsx_component_fixture_extracts_imports() {
    let source = fixture("typescript/component.tsx");
    let file = extract_ts(TsFlavor::Tsx, &source);
    let imports: Vec<&str> = file
        .base
        .refs
        .iter()
        .filter(|r| matches!(r.kind, RefKind::ImportEsm))
        .map(|r| r.name.as_str())
        .collect();
    assert!(imports.contains(&"react"));
    assert!(imports.contains(&"./ui/Button"));
}
