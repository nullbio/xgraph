use std::path::Path;
use std::sync::{Arc, OnceLock};

use tree_sitter::{Language, Node as TsNode, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::extract::{Diagnostic, ExtractedFile, LocalNodeId, Node, Position, Ref, Severity, Span};
use crate::language::{LanguageId, LanguagePlugin, LanguageQueries};

const DEFINITIONS_QUERY: &str = r#"
(import_statement) @import
(import_from_statement) @import_from
(class_definition) @class
(function_definition) @function
(decorated_definition) @decorated
"#;

static QUERIES: LanguageQueries = LanguageQueries {
    definitions: DEFINITIONS_QUERY,
    imports: "",
    exports: "",
    types: "",
    routes: "",
};

fn span_from_node(node: TsNode<'_>) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span {
        start: Position {
            byte: node.start_byte(),
            row: start.row,
            column: start.column,
        },
        end: Position {
            byte: node.end_byte(),
            row: end.row,
            column: end.column,
        },
    }
}

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

struct Extractor<'a> {
    out: ExtractedFile,
    source: &'a [u8],
    next_node_id: u32,
    next_ref_id: u32,
    /// `(start_byte, end_byte, local_id)` for each top-level / nested def.
    /// Used by `enclosing_def` so call refs and decorator refs get
    /// attributed to their containing function/class instead of dropping
    /// out at edge-resolution time.
    container_ranges: Vec<(usize, usize, LocalNodeId)>,
}

impl<'a> Extractor<'a> {
    fn new(source: &'a [u8], path: &Path) -> Self {
        Self {
            out: ExtractedFile {
                path: path.to_path_buf(),
                ..ExtractedFile::default()
            },
            source,
            next_node_id: 0,
            next_ref_id: 0,
            container_ranges: Vec::new(),
        }
    }

    fn enclosing_def(&self, start: usize, end: usize) -> Option<LocalNodeId> {
        let mut best: Option<(usize, LocalNodeId)> = None;
        for &(c_start, c_end, id) in &self.container_ranges {
            if c_start <= start && c_end >= end && (c_start < start || c_end > end) {
                let span = c_end - c_start;
                if best.as_ref().is_none_or(|(s, _)| span < *s) {
                    best = Some((span, id));
                }
            }
        }
        best.map(|(_, id)| id)
    }

    fn push_node(
        &mut self,
        kind: &'static str,
        name: String,
        qname: String,
        span: Span,
        parent: Option<LocalNodeId>,
        byte_range: (usize, usize),
    ) -> LocalNodeId {
        let id = self.next_node_id;
        self.next_node_id += 1;
        self.container_ranges.push((byte_range.0, byte_range.1, id));
        self.out.nodes.push(Node {
            id,
            kind: kind.to_string(),
            name,
            qname,
            span,
            parent,
        });
        id
    }

    fn push_ref(
        &mut self,
        kind: &'static str,
        name: String,
        qname: Option<String>,
        alias: Option<String>,
        span: Span,
        container: Option<LocalNodeId>,
    ) {
        let id = self.next_ref_id;
        self.next_ref_id += 1;
        self.out.refs.push(Ref {
            id,
            kind: kind.to_string(),
            name,
            qname,
            alias,
            span,
            container,
        });
    }
}

pub struct PythonPlugin;

impl LanguagePlugin for PythonPlugin {
    fn id(&self) -> LanguageId {
        LanguageId::Python
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    fn queries(&self) -> &'static LanguageQueries {
        &QUERIES
    }

    fn tree_sitter_language(&self) -> Language {
        python_language()
    }

    fn extract(&self, source: &[u8], path: &Path) -> ExtractedFile {
        let Some(tree) = parse(source) else {
            let mut out = ExtractedFile {
                path: path.to_path_buf(),
                ..ExtractedFile::default()
            };
            out.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message: "python parser failed to produce a tree".into(),
                span: None,
            });
            return out;
        };

        let mut extractor = Extractor::new(source, path);
        let root = tree.root_node();
        run_definitions_query(&mut extractor, root);
        collect_module_constants(&mut extractor, root);
        walk_for_calls(&mut extractor, root);
        collect_errors(&mut extractor, root);
        extractor.out
    }
}

thread_local! {
    static PARSER: std::cell::RefCell<Option<Parser>> = const { std::cell::RefCell::new(None) };
}

pub fn parse(source: &[u8]) -> Option<Tree> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut p = Parser::new();
            p.set_language(&python_language()).ok()?;
            *slot = Some(p);
        }
        slot.as_mut().unwrap().parse(source, None)
    })
}

pub fn extract(source: &[u8]) -> ExtractedFile {
    PythonPlugin.extract(source, Path::new(""))
}

fn slice_text<'a>(node: TsNode<'_>, source: &'a [u8]) -> &'a str {
    let start = node.start_byte().min(source.len());
    let end = node.end_byte().min(source.len());
    if start > end {
        return "";
    }
    std::str::from_utf8(&source[start..end]).unwrap_or("")
}

fn run_definitions_query(extractor: &mut Extractor<'_>, root: TsNode<'_>) {
    let Some(query) = query() else {
        extractor.out.diagnostics.push(Diagnostic {
            severity: Severity::Error,
            message: "python definitions query failed to compile".into(),
            span: Some(span_from_node(root)),
        });
        return;
    };
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, extractor.source);
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            match *capture_name {
                "import" => collect_import_statement(extractor, node),
                "import_from" => collect_import_from_statement(extractor, node),
                "class" if !is_inside_decorated(node) && !is_inside_class_body(node) => {
                    collect_class(extractor, node, None);
                }
                "function" if !is_inside_decorated(node) && !is_inside_class_body(node) => {
                    collect_function(extractor, node, None, "function");
                }
                "decorated" if !is_inside_class_body(node) => {
                    collect_decorated(extractor, node, None);
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

fn collect_import_statement(extractor: &mut Extractor<'_>, node: TsNode<'_>) {
    let span = span_from_node(node);
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        let (module_name, alias) = decode_import_name(child, extractor.source);
        extractor.push_ref("import", module_name, None, alias, span, None);
    }
}

fn collect_import_from_statement(extractor: &mut Extractor<'_>, node: TsNode<'_>) {
    let span = span_from_node(node);
    let Some(module_field) = node.child_by_field_name("module_name") else {
        return;
    };
    let module_name = match module_field.kind() {
        "dotted_name" => slice_text(module_field, extractor.source).to_string(),
        "relative_import" => decode_relative_import(module_field, extractor.source),
        _ => slice_text(module_field, extractor.source).to_string(),
    };

    let mut emitted = false;
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        let (item_name, alias) = decode_import_name(child, extractor.source);
        let qname = format!("{module_name}.{item_name}");
        extractor.push_ref("import", item_name, Some(qname), alias, span, None);
        emitted = true;
    }

    if !emitted {
        let mut wildcard_cursor = node.walk();
        for child in node.children(&mut wildcard_cursor) {
            if child.kind() == "wildcard_import" {
                let qname = format!("{module_name}.*");
                extractor.push_ref("import", "*".to_string(), Some(qname), None, span, None);
            }
        }
    }
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

fn collect_decorated(extractor: &mut Extractor<'_>, node: TsNode<'_>, parent: Option<LocalNodeId>) {
    let Some(def) = node.child_by_field_name("definition") else {
        return;
    };

    let def_id = match def.kind() {
        "class_definition" => Some(collect_class(extractor, def, parent)),
        "function_definition" => {
            let kind = if is_inside_class_body(node) {
                "method"
            } else {
                "function"
            };
            Some(collect_function(extractor, def, parent, kind))
        }
        _ => None,
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            let expr_text = decorator_expression_text(child, extractor.source);
            if !expr_text.is_empty() {
                let container = def_id.or_else(|| {
                    extractor.enclosing_def(child.start_byte(), child.end_byte())
                });
                extractor.push_ref(
                    "decorator",
                    expr_text,
                    None,
                    None,
                    span_from_node(child),
                    container,
                );
            }
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

fn collect_class(
    extractor: &mut Extractor<'_>,
    node: TsNode<'_>,
    parent: Option<LocalNodeId>,
) -> LocalNodeId {
    let name_node = node.child_by_field_name("name");
    let name = name_node
        .map(|n| slice_text(n, extractor.source).to_string())
        .unwrap_or_default();
    let qname = qualified_name(node, &name, extractor.source);
    let span = span_from_node(node);

    let id = extractor.push_node(
        "class",
        name,
        qname,
        span,
        parent,
        (node.start_byte(), node.end_byte()),
    );

    if let Some(superclasses) = node.child_by_field_name("superclasses") {
        let mut cursor = superclasses.walk();
        for child in superclasses.children(&mut cursor) {
            if !child.is_named() {
                continue;
            }
            let text = slice_text(child, extractor.source).trim().to_string();
            if !text.is_empty() {
                extractor.push_ref(
                    "inheritance",
                    text,
                    None,
                    None,
                    span_from_node(child),
                    Some(id),
                );
            }
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        collect_class_body(extractor, body, id);
    }

    id
}

fn collect_class_body(extractor: &mut Extractor<'_>, body: TsNode<'_>, parent: LocalNodeId) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                collect_function(extractor, child, Some(parent), "method");
            }
            "decorated_definition" => {
                collect_decorated(extractor, child, Some(parent));
            }
            "class_definition" => {
                collect_class(extractor, child, Some(parent));
            }
            _ => {}
        }
    }
}

fn collect_function(
    extractor: &mut Extractor<'_>,
    node: TsNode<'_>,
    parent: Option<LocalNodeId>,
    kind: &'static str,
) -> LocalNodeId {
    let name_node = node.child_by_field_name("name");
    let name = name_node
        .map(|n| slice_text(n, extractor.source).to_string())
        .unwrap_or_default();
    let qname = qualified_name(node, &name, extractor.source);
    let span = span_from_node(node);

    extractor.push_node(
        kind,
        name,
        qname,
        span,
        parent,
        (node.start_byte(), node.end_byte()),
    )
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

fn collect_module_constants(extractor: &mut Extractor<'_>, root: TsNode<'_>) {
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
            let name = slice_text(left, extractor.source).to_string();
            if !is_screaming_snake_case(&name) {
                continue;
            }
            let qname = name.clone();
            extractor.push_node(
                "constant",
                name,
                qname,
                span_from_node(stmt),
                None,
                (stmt.start_byte(), stmt.end_byte()),
            );
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

fn walk_for_calls(extractor: &mut Extractor<'_>, root: TsNode<'_>) {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "call"
            && let Some(callee) = node.child_by_field_name("function")
        {
            let chain = member_chain(callee, extractor.source);
            if !chain.is_empty() {
                let display = chain.join(".");
                let container = extractor.enclosing_def(node.start_byte(), node.end_byte());
                extractor.push_ref(
                    "call",
                    display,
                    None,
                    None,
                    span_from_node(node),
                    container,
                );
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

fn collect_errors(extractor: &mut Extractor<'_>, root: TsNode<'_>) {
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
            extractor.out.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message,
                span: Some(span_from_node(node)),
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
        out.refs.iter().filter(|r| r.kind == "import").collect()
    }

    fn calls(out: &ExtractedFile) -> Vec<&Ref> {
        out.refs.iter().filter(|r| r.kind == "call").collect()
    }

    fn decorators(out: &ExtractedFile) -> Vec<&Ref> {
        out.refs.iter().filter(|r| r.kind == "decorator").collect()
    }

    fn inheritance(out: &ExtractedFile) -> Vec<&Ref> {
        out.refs
            .iter()
            .filter(|r| r.kind == "inheritance")
            .collect()
    }

    #[test]
    fn parses_and_extracts_module_functions() {
        let src = b"def greet(name):\n    print(name)\n";
        let out = extract(src);
        let func = find_node(&out, "greet").expect("function captured");
        assert_eq!(func.kind, "function");
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
        assert!(os.qname.is_none());
        assert!(os.alias.is_none());

        let list = imports
            .iter()
            .find(|r| r.name == "List")
            .expect("List import");
        assert_eq!(list.qname.as_deref(), Some("typing.List"));

        let foo = imports
            .iter()
            .find(|r| r.name == "foo")
            .expect("foo import");
        assert_eq!(foo.qname.as_deref(), Some(".helpers.foo"));

        let wildcard = imports
            .iter()
            .find(|r| r.name == "*")
            .expect("wildcard import");
        assert_eq!(wildcard.qname.as_deref(), Some("..pkg.*"));
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
        assert_eq!(numpy.alias.as_deref(), Some("np"));

        let list = imports
            .iter()
            .find(|r| r.name == "List")
            .expect("List import");
        assert_eq!(list.qname.as_deref(), Some("typing.List"));
        assert_eq!(list.alias.as_deref(), Some("L"));
    }

    #[test]
    fn class_inheritance_and_methods_are_captured() {
        let src = b"class User(BaseUser):\n    def __init__(self):\n        self.x = 1\n    def name(self):\n        return self.x\n";
        let out = extract(src);

        let class = find_node(&out, "User").expect("class captured");
        assert_eq!(class.kind, "class");

        let bases = inheritance(&out);
        assert!(bases.iter().any(|r| r.name == "BaseUser"));

        let ctor = find_node(&out, "__init__").expect("init captured");
        assert_eq!(ctor.kind, "method");
        assert_eq!(ctor.qname, "User.__init__");
        assert_eq!(ctor.parent, Some(class.id));

        let name = find_node(&out, "name").expect("name method captured");
        assert_eq!(name.kind, "method");
        assert_eq!(name.qname, "User.name");
        assert_eq!(name.parent, Some(class.id));
    }

    #[test]
    fn decorated_function_is_captured_with_decorator_ref() {
        let src = b"@app.route('/')\ndef index():\n    return 'hi'\n";
        let out = extract(src);

        let index = find_node(&out, "index").expect("index function captured");
        assert_eq!(index.kind, "function");

        let decos = decorators(&out);
        let route = decos
            .iter()
            .find(|d| d.name == "app.route('/')")
            .expect("decorator ref captured");
        assert_eq!(route.container, Some(index.id));
    }

    #[test]
    fn decorated_class_is_captured_with_decorator_ref() {
        let src = b"@register\nclass Widget(Base):\n    pass\n";
        let out = extract(src);

        let widget = find_node(&out, "Widget").expect("class captured");
        assert_eq!(widget.kind, "class");

        let decos = decorators(&out);
        let register = decos
            .iter()
            .find(|d| d.name == "register")
            .expect("register decorator captured");
        assert_eq!(register.container, Some(widget.id));
    }

    #[test]
    fn async_function_is_captured() {
        let src = b"async def fetch():\n    return 1\n";
        let out = extract(src);
        let fetch = find_node(&out, "fetch").expect("fetch captured");
        assert_eq!(fetch.kind, "function");
    }

    #[test]
    fn async_method_is_captured() {
        let src = b"class Client:\n    async def fetch(self):\n        return 1\n";
        let out = extract(src);
        let fetch = find_node(&out, "fetch").expect("fetch captured");
        assert_eq!(fetch.kind, "method");
    }

    #[test]
    fn chained_call_collects_full_member_chain() {
        let src = b"a.b.c()\n";
        let out = extract(src);
        let calls = calls(&out);
        let call = calls
            .iter()
            .find(|c| c.name == "a.b.c")
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
        let consts: Vec<_> = out.nodes.iter().filter(|n| n.kind == "constant").collect();
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
        assert_eq!(good.kind, "function");
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
        assert_eq!(plugin.id(), LanguageId::Python);
        assert_eq!(plugin.extensions(), &["py", "pyi"]);
    }
}
