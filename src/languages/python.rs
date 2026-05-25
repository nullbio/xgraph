use std::sync::{Arc, OnceLock};

use tree_sitter::{Language, Node as TsNode, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

impl Span {
    fn from_node(node: TsNode<'_>) -> Self {
        let start = node.start_position();
        let end = node.end_position();
        Self {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_row: start.row,
            start_col: start.column,
            end_row: end.row,
            end_col: end.column,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Function,
    Method,
    Class,
    Constant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    pub name: String,
    pub qname: String,
    pub span: Span,
    pub is_async: bool,
    pub bases: Vec<String>,
    pub decorators: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Import,
    Inheritance,
    Call,
    Decorator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub kind: RefKind,
    pub name: String,
    pub items: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    pub span: Span,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedFile {
    pub nodes: Vec<Node>,
    pub refs: Vec<Ref>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LanguageId(pub &'static str);

pub trait LanguagePlugin {
    fn id(&self) -> LanguageId;
    fn extensions(&self) -> &[&'static str];
    fn tree_sitter_language(&self) -> Language;
    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile;
}

const DEFINITIONS_QUERY: &str = r#"
(import_statement) @import
(import_from_statement) @import_from
(class_definition) @class
(function_definition) @function
(decorated_definition) @decorated
"#;

fn query() -> Option<Arc<Query>> {
    static QUERY: OnceLock<Option<Arc<Query>>> = OnceLock::new();
    QUERY
        .get_or_init(|| {
            let language = python_language();
            Query::new(&language, DEFINITIONS_QUERY).ok().map(Arc::new)
        })
        .clone()
}

fn python_language() -> Language {
    tree_sitter_python::LANGUAGE.into()
}

pub struct PythonPlugin;

impl LanguagePlugin for PythonPlugin {
    fn id(&self) -> LanguageId {
        LanguageId("python")
    }

    fn extensions(&self) -> &[&'static str] {
        &["py", "pyi"]
    }

    fn tree_sitter_language(&self) -> Language {
        python_language()
    }

    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile {
        let mut out = ExtractedFile::default();
        let root = tree.root_node();

        run_definitions_query(&mut out, root, source);
        collect_module_constants(&mut out, root, source);
        walk_for_calls(&mut out, root, source);
        collect_errors(&mut out, root);

        out
    }
}

pub fn parse(source: &[u8]) -> Option<Tree> {
    let mut parser = Parser::new();
    let language = python_language();
    parser.set_language(&language).ok()?;
    parser.parse(source, None)
}

pub fn extract(source: &[u8]) -> ExtractedFile {
    let Some(tree) = parse(source) else {
        let span = Span {
            start_byte: 0,
            end_byte: source.len(),
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 0,
        };
        return ExtractedFile {
            nodes: Vec::new(),
            refs: Vec::new(),
            diagnostics: vec![Diagnostic {
                message: "python parser failed to produce a tree".into(),
                span,
            }],
        };
    };
    PythonPlugin.extract(&tree, source)
}

fn slice_text<'a>(node: TsNode<'_>, source: &'a [u8]) -> &'a str {
    let start = node.start_byte().min(source.len());
    let end = node.end_byte().min(source.len());
    if start > end {
        return "";
    }
    std::str::from_utf8(&source[start..end]).unwrap_or("")
}

fn run_definitions_query(out: &mut ExtractedFile, root: TsNode<'_>, source: &[u8]) {
    let Some(query) = query() else {
        out.diagnostics.push(Diagnostic {
            message: "python definitions query failed to compile".into(),
            span: Span::from_node(root),
        });
        return;
    };
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            match *capture_name {
                "import" => collect_import_statement(out, node, source),
                "import_from" => collect_import_from_statement(out, node, source),
                "class" if !is_inside_decorated(node) && !is_inside_class_body(node) => {
                    collect_class(out, node, source, &[]);
                }
                "function" if !is_inside_decorated(node) && !is_inside_class_body(node) => {
                    collect_function(out, node, source, &[], NodeKind::Function);
                }
                "decorated" if !is_inside_class_body(node) => {
                    collect_decorated(out, node, source);
                }
                _ => {}
            }
        }
    }
}

fn is_inside_decorated(node: TsNode<'_>) -> bool {
    node.parent()
        .map(|p| p.kind() == "decorated_definition")
        .unwrap_or(false)
}

fn is_inside_class_body(node: TsNode<'_>) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "class_definition" => return true,
            "function_definition" | "module" => return false,
            _ => current = n.parent(),
        }
    }
    false
}

fn collect_import_statement(out: &mut ExtractedFile, node: TsNode<'_>, source: &[u8]) {
    let span = Span::from_node(node);
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        let (module_name, alias) = decode_import_name(child, source);
        let items = match alias {
            Some(a) => vec![format!("{module_name} as {a}")],
            None => vec![module_name.clone()],
        };
        out.refs.push(Ref {
            kind: RefKind::Import,
            name: module_name,
            items,
            span,
        });
    }
}

fn collect_import_from_statement(out: &mut ExtractedFile, node: TsNode<'_>, source: &[u8]) {
    let span = Span::from_node(node);
    let Some(module_field) = node.child_by_field_name("module_name") else {
        return;
    };
    let module_name = match module_field.kind() {
        "dotted_name" => slice_text(module_field, source).to_string(),
        "relative_import" => decode_relative_import(module_field, source),
        _ => slice_text(module_field, source).to_string(),
    };

    let mut items: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        let (item_name, alias) = decode_import_name(child, source);
        match alias {
            Some(a) => items.push(format!("{item_name} as {a}")),
            None => items.push(item_name),
        }
    }

    if items.is_empty() {
        let mut wildcard_cursor = node.walk();
        for child in node.children(&mut wildcard_cursor) {
            if child.kind() == "wildcard_import" {
                items.push("*".into());
            }
        }
    }

    out.refs.push(Ref {
        kind: RefKind::Import,
        name: module_name,
        items,
        span,
    });
}

fn decode_import_name(node: TsNode<'_>, source: &[u8]) -> (String, Option<String>) {
    match node.kind() {
        "aliased_import" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| slice_text(n, source).to_string())
                .unwrap_or_default();
            let alias = node
                .child_by_field_name("alias")
                .map(|n| slice_text(n, source).to_string());
            (name, alias)
        }
        _ => (slice_text(node, source).to_string(), None),
    }
}

fn decode_relative_import(node: TsNode<'_>, source: &[u8]) -> String {
    let mut text = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_prefix" => text.push_str(slice_text(child, source)),
            "dotted_name" => text.push_str(slice_text(child, source)),
            _ => {}
        }
    }
    text
}

fn collect_decorated(out: &mut ExtractedFile, node: TsNode<'_>, source: &[u8]) {
    let mut decorators: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            let expr_text = decorator_expression_text(child, source);
            if !expr_text.is_empty() {
                decorators.push(expr_text.clone());
                out.refs.push(Ref {
                    kind: RefKind::Decorator,
                    name: expr_text,
                    items: Vec::new(),
                    span: Span::from_node(child),
                });
            }
        }
    }

    if let Some(def) = node.child_by_field_name("definition") {
        match def.kind() {
            "class_definition" => collect_class(out, def, source, &decorators),
            "function_definition" => {
                let kind = if is_inside_class_body(node) {
                    NodeKind::Method
                } else {
                    NodeKind::Function
                };
                collect_function(out, def, source, &decorators, kind);
            }
            _ => {}
        }
    }
}

fn decorator_expression_text(decorator: TsNode<'_>, source: &[u8]) -> String {
    let mut cursor = decorator.walk();
    for child in decorator.children(&mut cursor) {
        if child.is_named() {
            return slice_text(child, source).to_string();
        }
    }
    String::new()
}

fn collect_class(out: &mut ExtractedFile, node: TsNode<'_>, source: &[u8], decorators: &[String]) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = slice_text(name_node, source).to_string();
    let qname = qualified_name(node, &name, source);
    let span = Span::from_node(node);

    let mut bases: Vec<String> = Vec::new();
    if let Some(superclasses) = node.child_by_field_name("superclasses") {
        let mut cursor = superclasses.walk();
        for child in superclasses.children(&mut cursor) {
            if !child.is_named() {
                continue;
            }
            let text = slice_text(child, source).trim().to_string();
            if !text.is_empty() {
                bases.push(text.clone());
                out.refs.push(Ref {
                    kind: RefKind::Inheritance,
                    name: text,
                    items: Vec::new(),
                    span: Span::from_node(child),
                });
            }
        }
    }

    out.nodes.push(Node {
        kind: NodeKind::Class,
        name,
        qname,
        span,
        is_async: false,
        bases,
        decorators: decorators.to_vec(),
    });

    if let Some(body) = node.child_by_field_name("body") {
        collect_class_body(out, body, source);
    }
}

fn collect_class_body(out: &mut ExtractedFile, body: TsNode<'_>, source: &[u8]) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                collect_function(out, child, source, &[], NodeKind::Method);
            }
            "decorated_definition" => {
                collect_decorated(out, child, source);
            }
            "class_definition" => {
                collect_class(out, child, source, &[]);
            }
            _ => {}
        }
    }
}

fn collect_function(
    out: &mut ExtractedFile,
    node: TsNode<'_>,
    source: &[u8],
    decorators: &[String],
    kind: NodeKind,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = slice_text(name_node, source).to_string();
    let qname = qualified_name(node, &name, source);
    let span = Span::from_node(node);
    let is_async = function_is_async(node, source);

    out.nodes.push(Node {
        kind,
        name,
        qname,
        span,
        is_async,
        bases: Vec::new(),
        decorators: decorators.to_vec(),
    });
}

fn function_is_async(node: TsNode<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            continue;
        }
        if slice_text(child, source) == "async" {
            return true;
        }
        if slice_text(child, source) == "def" {
            break;
        }
    }
    false
}

fn qualified_name(node: TsNode<'_>, leaf: &str, source: &[u8]) -> String {
    let mut parts: Vec<String> = vec![leaf.to_string()];
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "class_definition" | "function_definition" => {
                if let Some(name_node) = n.child_by_field_name("name") {
                    let text = slice_text(name_node, source).to_string();
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
            "module" => break,
            _ => {}
        }
        current = n.parent();
    }
    parts.reverse();
    parts.join(".")
}

fn collect_module_constants(out: &mut ExtractedFile, root: TsNode<'_>, source: &[u8]) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "expression_statement" {
            continue;
        }
        let mut inner = child.walk();
        for stmt in child.children(&mut inner) {
            if stmt.kind() != "assignment" {
                continue;
            }
            let Some(left) = stmt.child_by_field_name("left") else {
                continue;
            };
            if left.kind() != "identifier" {
                continue;
            }
            let name = slice_text(left, source).to_string();
            if !is_screaming_snake_case(&name) {
                continue;
            }
            out.nodes.push(Node {
                kind: NodeKind::Constant,
                name: name.clone(),
                qname: name,
                span: Span::from_node(stmt),
                is_async: false,
                bases: Vec::new(),
                decorators: Vec::new(),
            });
        }
    }
}

fn is_screaming_snake_case(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && s.chars().any(|c| c.is_ascii_uppercase())
}

fn walk_for_calls(out: &mut ExtractedFile, root: TsNode<'_>, source: &[u8]) {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "call"
            && let Some(callee) = node.child_by_field_name("function")
        {
            let chain = member_chain(callee, source);
            if !chain.is_empty() {
                let display = chain.join(".");
                out.refs.push(Ref {
                    kind: RefKind::Call,
                    name: display,
                    items: chain,
                    span: Span::from_node(node),
                });
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn member_chain(node: TsNode<'_>, source: &[u8]) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" => {
                parts.push(slice_text(current, source).to_string());
                break;
            }
            "attribute" => {
                if let Some(attr) = current.child_by_field_name("attribute") {
                    parts.push(slice_text(attr, source).to_string());
                }
                let Some(object) = current.child_by_field_name("object") else {
                    break;
                };
                current = object;
            }
            "call" => {
                let Some(func) = current.child_by_field_name("function") else {
                    break;
                };
                current = func;
            }
            _ => break,
        }
    }
    parts.reverse();
    parts
}

fn collect_errors(out: &mut ExtractedFile, root: TsNode<'_>) {
    if !root.has_error() {
        return;
    }
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            let message = if node.is_missing() {
                format!("missing token: {}", node.kind())
            } else {
                "syntax error".to_string()
            };
            out.diagnostics.push(Diagnostic {
                message,
                span: Span::from_node(node),
            });
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_node<'a>(out: &'a ExtractedFile, name: &str) -> Option<&'a Node> {
        out.nodes.iter().find(|n| n.name == name)
    }

    fn imports(out: &ExtractedFile) -> Vec<&Ref> {
        out.refs
            .iter()
            .filter(|r| r.kind == RefKind::Import)
            .collect()
    }

    fn calls(out: &ExtractedFile) -> Vec<&Ref> {
        out.refs
            .iter()
            .filter(|r| r.kind == RefKind::Call)
            .collect()
    }

    fn decorators(out: &ExtractedFile) -> Vec<&Ref> {
        out.refs
            .iter()
            .filter(|r| r.kind == RefKind::Decorator)
            .collect()
    }

    fn inheritance(out: &ExtractedFile) -> Vec<&Ref> {
        out.refs
            .iter()
            .filter(|r| r.kind == RefKind::Inheritance)
            .collect()
    }

    #[test]
    fn parses_and_extracts_module_functions() {
        let src = b"def greet(name):\n    print(name)\n";
        let out = extract(src);
        let func = find_node(&out, "greet").expect("function captured");
        assert_eq!(func.kind, NodeKind::Function);
        assert!(!func.is_async);
        assert_eq!(func.qname, "greet");
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn captures_plain_and_from_imports() {
        let src =
            b"import os\nfrom typing import List\nfrom .helpers import foo\nfrom ..pkg import *\n";
        let out = extract(src);
        let imports = imports(&out);
        assert_eq!(imports.len(), 4);

        let os = imports.iter().find(|r| r.name == "os").expect("os import");
        assert_eq!(os.items, vec!["os".to_string()]);

        let typing = imports
            .iter()
            .find(|r| r.name == "typing")
            .expect("typing import");
        assert_eq!(typing.items, vec!["List".to_string()]);

        let helpers = imports
            .iter()
            .find(|r| r.name == ".helpers")
            .expect("relative helpers import");
        assert_eq!(helpers.items, vec!["foo".to_string()]);

        let pkg = imports
            .iter()
            .find(|r| r.name == "..pkg")
            .expect("relative pkg wildcard import");
        assert_eq!(pkg.items, vec!["*".to_string()]);
    }

    #[test]
    fn import_alias_is_preserved() {
        let src = b"import numpy as np\nfrom typing import List as L\n";
        let out = extract(src);
        let imports = imports(&out);

        let numpy = imports
            .iter()
            .find(|r| r.name == "numpy")
            .expect("numpy import");
        assert_eq!(numpy.items, vec!["numpy as np".to_string()]);

        let typing = imports
            .iter()
            .find(|r| r.name == "typing")
            .expect("typing import");
        assert_eq!(typing.items, vec!["List as L".to_string()]);
    }

    #[test]
    fn class_inheritance_and_methods_are_captured() {
        let src = b"class User(BaseUser):\n    def __init__(self):\n        self.x = 1\n    def name(self):\n        return self.x\n";
        let out = extract(src);

        let class = find_node(&out, "User").expect("class captured");
        assert_eq!(class.kind, NodeKind::Class);
        assert_eq!(class.bases, vec!["BaseUser".to_string()]);

        let bases = inheritance(&out);
        assert!(bases.iter().any(|r| r.name == "BaseUser"));

        let ctor = find_node(&out, "__init__").expect("init captured");
        assert_eq!(ctor.kind, NodeKind::Method);
        assert_eq!(ctor.qname, "User.__init__");

        let name = find_node(&out, "name").expect("name method captured");
        assert_eq!(name.kind, NodeKind::Method);
        assert_eq!(name.qname, "User.name");
    }

    #[test]
    fn decorated_function_is_captured_with_decorator_ref() {
        let src = b"@app.route('/')\ndef index():\n    return 'hi'\n";
        let out = extract(src);

        let index = find_node(&out, "index").expect("index function captured");
        assert_eq!(index.decorators, vec!["app.route('/')".to_string()]);

        let decos = decorators(&out);
        assert!(decos.iter().any(|d| d.name == "app.route('/')"));
    }

    #[test]
    fn decorated_class_is_captured_with_decorator_ref() {
        let src = b"@register\nclass Widget(Base):\n    pass\n";
        let out = extract(src);

        let widget = find_node(&out, "Widget").expect("class captured");
        assert_eq!(widget.kind, NodeKind::Class);
        assert_eq!(widget.decorators, vec!["register".to_string()]);

        let decos = decorators(&out);
        assert!(decos.iter().any(|d| d.name == "register"));
    }

    #[test]
    fn async_function_is_flagged() {
        let src = b"async def fetch():\n    return 1\n";
        let out = extract(src);
        let fetch = find_node(&out, "fetch").expect("fetch captured");
        assert!(fetch.is_async, "expected async flag to be set");
    }

    #[test]
    fn async_method_is_flagged() {
        let src = b"class Client:\n    async def fetch(self):\n        return 1\n";
        let out = extract(src);
        let fetch = find_node(&out, "fetch").expect("fetch captured");
        assert_eq!(fetch.kind, NodeKind::Method);
        assert!(fetch.is_async);
    }

    #[test]
    fn chained_call_collects_full_member_chain() {
        let src = b"a.b.c()\n";
        let out = extract(src);
        let calls = calls(&out);
        let call = calls
            .iter()
            .find(|c| c.items == vec!["a".to_string(), "b".to_string(), "c".to_string()])
            .expect("full chain captured");
        assert_eq!(call.name, "a.b.c");
    }

    #[test]
    fn simple_call_captured() {
        let src = b"print('hi')\n";
        let out = extract(src);
        let calls = calls(&out);
        assert!(calls.iter().any(|c| c.name == "print"));
    }

    #[test]
    fn module_constant_is_captured() {
        let src = b"MAX_RETRIES = 5\nname = 'x'\n";
        let out = extract(src);
        let consts: Vec<_> = out
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Constant)
            .collect();
        assert_eq!(consts.len(), 1);
        assert_eq!(consts[0].name, "MAX_RETRIES");
    }

    #[test]
    fn malformed_input_yields_diagnostic_and_partial_extraction() {
        let src = b"def good():\n    return 1\ndef bad(:\n    return 2\n";
        let out = extract(src);
        assert!(
            !out.diagnostics.is_empty(),
            "expected at least one diagnostic"
        );
        let good = find_node(&out, "good").expect("good function still captured");
        assert_eq!(good.kind, NodeKind::Function);
    }

    #[test]
    fn shared_query_is_cached() {
        let q1 = query().expect("query must compile");
        let q2 = query().expect("query must compile");
        assert!(Arc::ptr_eq(&q1, &q2));
    }

    #[test]
    fn plugin_reports_python_extensions() {
        let plugin = PythonPlugin;
        assert_eq!(plugin.id().0, "python");
        assert_eq!(plugin.extensions(), &["py", "pyi"]);
    }
}
