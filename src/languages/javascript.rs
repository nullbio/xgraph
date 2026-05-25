use std::sync::{Arc, OnceLock};

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    JavaScript,
    TypeScript,
    Tsx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Module,
    Class,
    Function,
    Method,
    ArrowFunction,
    Variable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    ImportEsm,
    ImportCjs,
    ExportEsm,
    ExportCjs,
    Call,
    MemberAccess,
    JsxComponent,
}

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
    pub(super) fn from_node(node: Node<'_>) -> Self {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedNode {
    pub kind: NodeKind,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub kind: RefKind,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub span: Span,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtractedFile {
    pub language: Option<LanguageId>,
    pub nodes: Vec<ExtractedNode>,
    pub refs: Vec<Ref>,
    pub diagnostics: Vec<Diagnostic>,
}

pub trait LanguagePlugin {
    fn id(&self) -> LanguageId;
    fn extensions(&self) -> &'static [&'static str];
    fn tree_sitter_language(&self) -> Language;
    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile;
}

const DEFINITIONS_QUERY: &str = r#"
(class_declaration
  name: (identifier) @class.name) @class.def

(function_declaration
  name: (identifier) @function.name) @function.def

(generator_function_declaration
  name: (identifier) @function.name) @function.def

(variable_declarator
  name: (identifier) @var.name) @var.def

(method_definition
  name: (property_identifier) @method.name) @method.def
"#;

const IMPORT_QUERY: &str = r#"
(import_statement
  source: (string) @import.source) @import

(call_expression
  function: (identifier) @cjs.fn
  arguments: (arguments . (string) @cjs.source))
"#;

const EXPORT_QUERY: &str = r#"
(export_statement) @export

(assignment_expression
  left: (member_expression
    object: (identifier) @cjs.obj
    property: (property_identifier) @cjs.prop)) @cjs.export
"#;

pub struct JavaScriptPlugin;

impl JavaScriptPlugin {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for JavaScriptPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguagePlugin for JavaScriptPlugin {
    fn id(&self) -> LanguageId {
        LanguageId::JavaScript
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["js", "jsx", "cjs", "mjs"]
    }

    fn tree_sitter_language(&self) -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }

    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile {
        extract_internal(tree, source)
    }
}

fn language() -> &'static Language {
    static LANG: OnceLock<Language> = OnceLock::new();
    LANG.get_or_init(|| tree_sitter_javascript::LANGUAGE.into())
}

fn definitions_query() -> &'static Arc<Query> {
    static QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    QUERY.get_or_init(|| {
        Arc::new(
            Query::new(language(), DEFINITIONS_QUERY)
                .unwrap_or_else(|err| panic!("invalid javascript definitions query: {err:?}")),
        )
    })
}

fn import_query() -> &'static Arc<Query> {
    static QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    QUERY.get_or_init(|| {
        Arc::new(
            Query::new(language(), IMPORT_QUERY)
                .unwrap_or_else(|err| panic!("invalid javascript import query: {err:?}")),
        )
    })
}

fn export_query() -> &'static Arc<Query> {
    static QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    QUERY.get_or_init(|| {
        Arc::new(
            Query::new(language(), EXPORT_QUERY)
                .unwrap_or_else(|err| panic!("invalid javascript export query: {err:?}")),
        )
    })
}

pub fn parse(source: &[u8]) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language()).ok()?;
    parser.parse(source, None)
}

pub fn extract(source: &[u8]) -> ExtractedFile {
    let Some(tree) = parse(source) else {
        return ExtractedFile {
            language: Some(LanguageId::JavaScript),
            ..Default::default()
        };
    };
    extract_internal(&tree, source)
}

fn extract_internal(tree: &Tree, source: &[u8]) -> ExtractedFile {
    let mut out = ExtractedFile {
        language: Some(LanguageId::JavaScript),
        ..Default::default()
    };
    let root = tree.root_node();
    collect_definitions(&root, source, &mut out.nodes);
    collect_imports(&root, source, &mut out.refs);
    collect_exports(&root, source, &mut out.refs);
    collect_calls_and_jsx(root, source, &mut out.refs);
    collect_diagnostics(root, &mut out.diagnostics);
    out
}

pub(super) fn slice_text(source: &[u8], node: Node<'_>) -> String {
    let bytes = &source[node.byte_range()];
    String::from_utf8_lossy(bytes).into_owned()
}

pub(super) fn strip_string_quotes(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'' || first == b'`') && first == last {
            return &raw[1..raw.len() - 1];
        }
    }
    raw
}

fn collect_definitions(root: &Node<'_>, source: &[u8], out: &mut Vec<ExtractedNode>) {
    let query = definitions_query();
    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut matches = cursor.matches(query, *root, source);
    while let Some(m) = matches.next() {
        let mut kind: Option<NodeKind> = None;
        let mut name: Option<String> = None;
        let mut def_node: Option<Node<'_>> = None;
        for cap in m.captures {
            let cname = names[cap.index as usize];
            match cname {
                "class.def" => {
                    kind = Some(NodeKind::Class);
                    def_node = Some(cap.node);
                }
                "function.def" => {
                    kind = Some(NodeKind::Function);
                    def_node = Some(cap.node);
                }
                "method.def" => {
                    kind = Some(NodeKind::Method);
                    def_node = Some(cap.node);
                }
                "var.def" => {
                    kind = Some(variable_kind_from_declarator(cap.node));
                    def_node = Some(cap.node);
                }
                "class.name" | "function.name" | "method.name" | "var.name" => {
                    name = Some(slice_text(source, cap.node));
                }
                _ => {}
            }
        }
        if let (Some(kind), Some(name), Some(node)) = (kind, name, def_node) {
            out.push(ExtractedNode {
                kind,
                name,
                span: Span::from_node(node),
            });
        }
    }
}

pub(super) fn variable_kind_from_declarator(declarator: Node<'_>) -> NodeKind {
    let Some(value) = declarator.child_by_field_name("value") else {
        return NodeKind::Variable;
    };
    match value.kind() {
        "arrow_function" => NodeKind::ArrowFunction,
        "function_expression" | "generator_function" => NodeKind::Function,
        "class" => NodeKind::Class,
        _ => NodeKind::Variable,
    }
}

fn collect_imports(root: &Node<'_>, source: &[u8], out: &mut Vec<Ref>) {
    let query = import_query();
    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut matches = cursor.matches(query, *root, source);
    while let Some(m) = matches.next() {
        let mut esm_source: Option<Node<'_>> = None;
        let mut esm_stmt: Option<Node<'_>> = None;
        let mut cjs_fn: Option<Node<'_>> = None;
        let mut cjs_source: Option<Node<'_>> = None;
        for cap in m.captures {
            match names[cap.index as usize] {
                "import.source" => esm_source = Some(cap.node),
                "import" => esm_stmt = Some(cap.node),
                "cjs.fn" => cjs_fn = Some(cap.node),
                "cjs.source" => cjs_source = Some(cap.node),
                _ => {}
            }
        }
        if let (Some(src), Some(stmt)) = (esm_source, esm_stmt) {
            let raw = slice_text(source, src);
            let name = strip_string_quotes(&raw).to_owned();
            out.push(Ref {
                kind: RefKind::ImportEsm,
                name,
                span: Span::from_node(stmt),
            });
        }
        if let (Some(func), Some(src)) = (cjs_fn, cjs_source) {
            let fn_name = slice_text(source, func);
            if fn_name == "require" {
                let raw = slice_text(source, src);
                let name = strip_string_quotes(&raw).to_owned();
                out.push(Ref {
                    kind: RefKind::ImportCjs,
                    name,
                    span: Span::from_node(src),
                });
            }
        }
    }
}

fn collect_exports(root: &Node<'_>, source: &[u8], out: &mut Vec<Ref>) {
    let query = export_query();
    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut matches = cursor.matches(query, *root, source);
    while let Some(m) = matches.next() {
        let mut esm_export: Option<Node<'_>> = None;
        let mut cjs_obj: Option<Node<'_>> = None;
        let mut cjs_prop: Option<Node<'_>> = None;
        let mut cjs_stmt: Option<Node<'_>> = None;
        for cap in m.captures {
            match names[cap.index as usize] {
                "export" => esm_export = Some(cap.node),
                "cjs.obj" => cjs_obj = Some(cap.node),
                "cjs.prop" => cjs_prop = Some(cap.node),
                "cjs.export" => cjs_stmt = Some(cap.node),
                _ => {}
            }
        }
        if let Some(node) = esm_export {
            let name = export_label(node, source);
            out.push(Ref {
                kind: RefKind::ExportEsm,
                name,
                span: Span::from_node(node),
            });
        }
        if let (Some(obj), Some(prop), Some(stmt)) = (cjs_obj, cjs_prop, cjs_stmt) {
            let obj_text = slice_text(source, obj);
            let prop_text = slice_text(source, prop);
            if obj_text == "module" && prop_text == "exports" {
                out.push(Ref {
                    kind: RefKind::ExportCjs,
                    name: "module.exports".to_owned(),
                    span: Span::from_node(stmt),
                });
            } else if obj_text == "exports" {
                out.push(Ref {
                    kind: RefKind::ExportCjs,
                    name: format!("exports.{prop_text}"),
                    span: Span::from_node(stmt),
                });
            }
        }
    }
}

fn export_label(export_node: Node<'_>, source: &[u8]) -> String {
    if let Some(decl) = export_node.child_by_field_name("declaration")
        && let Some(name) = declaration_name(decl, source)
    {
        return name;
    }
    if let Some(src) = export_node.child_by_field_name("source") {
        let raw = slice_text(source, src);
        return strip_string_quotes(&raw).to_owned();
    }
    if is_default_export(export_node) {
        return "default".to_owned();
    }
    "export".to_owned()
}

pub(super) fn declaration_name(decl: Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(name) = decl.child_by_field_name("name") {
        return Some(slice_text(source, name));
    }
    if matches!(decl.kind(), "lexical_declaration" | "variable_declaration") {
        let mut cursor = decl.walk();
        for child in decl.named_children(&mut cursor) {
            if child.kind() == "variable_declarator"
                && let Some(name) = child.child_by_field_name("name")
            {
                return Some(slice_text(source, name));
            }
        }
    }
    None
}

fn is_default_export(export_node: Node<'_>) -> bool {
    let mut cursor = export_node.walk();
    for child in export_node.children(&mut cursor) {
        if !child.is_named() && child.kind() == "default" {
            return true;
        }
    }
    false
}

pub(super) fn collect_calls_and_jsx(root: Node<'_>, source: &[u8], out: &mut Vec<Ref>) {
    walk_tree(root, |node| visit_call_or_jsx(node, source, out));
}

fn visit_call_or_jsx(node: Node<'_>, source: &[u8], out: &mut Vec<Ref>) {
    match node.kind() {
        "call_expression" => {
            if let Some(callee) = node.child_by_field_name("function")
                && !is_require_call(callee, source)
            {
                let name = callee_label(callee, source);
                if !name.is_empty() {
                    out.push(Ref {
                        kind: RefKind::Call,
                        name,
                        span: Span::from_node(node),
                    });
                }
            }
        }
        "member_expression" => {
            if let Some(prop) = node.child_by_field_name("property") {
                let name = slice_text(source, prop);
                out.push(Ref {
                    kind: RefKind::MemberAccess,
                    name,
                    span: Span::from_node(node),
                });
            }
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = slice_text(source, name_node);
                if is_component_name(&text) {
                    out.push(Ref {
                        kind: RefKind::JsxComponent,
                        name: text,
                        span: Span::from_node(node),
                    });
                }
            }
        }
        _ => {}
    }
}

pub(super) fn is_require_call(callee: Node<'_>, source: &[u8]) -> bool {
    callee.kind() == "identifier" && slice_text(source, callee) == "require"
}

pub(super) fn callee_label(callee: Node<'_>, source: &[u8]) -> String {
    match callee.kind() {
        "identifier" => slice_text(source, callee),
        "member_expression" => {
            if let Some(prop) = callee.child_by_field_name("property") {
                slice_text(source, prop)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

pub(super) fn is_component_name(name: &str) -> bool {
    let head = name.split(['.', ':']).next().unwrap_or("");
    head.chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_' || c == '$')
}

pub(super) fn collect_diagnostics(root: Node<'_>, out: &mut Vec<Diagnostic>) {
    if !root.has_error() {
        return;
    }
    walk_tree(root, |node| {
        if node.is_error() || node.is_missing() {
            out.push(Diagnostic {
                severity: DiagnosticSeverity::Error,
                message: if node.is_missing() {
                    format!("missing {}", node.kind())
                } else {
                    "syntax error".to_owned()
                },
                span: Span::from_node(node),
            });
        }
    });
}

pub(super) fn walk_tree<F: FnMut(Node<'_>)>(root: Node<'_>, mut visit: F) {
    let mut cursor = root.walk();
    'outer: loop {
        visit(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'outer;
            }
            if !cursor.goto_parent() {
                break 'outer;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_str(source: &str) -> ExtractedFile {
        extract(source.as_bytes())
    }

    fn refs_of_kind(file: &ExtractedFile, kind: RefKind) -> Vec<&Ref> {
        file.refs.iter().filter(|r| r.kind == kind).collect()
    }

    fn nodes_of_kind(file: &ExtractedFile, kind: NodeKind) -> Vec<&ExtractedNode> {
        file.nodes.iter().filter(|n| n.kind == kind).collect()
    }

    #[test]
    fn esm_default_import_extracts_source() {
        let file = extract_str("import x from 'mod';");
        let imports = refs_of_kind(&file, RefKind::ImportEsm);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn esm_named_imports_extract_source() {
        let file = extract_str("import {a, b} from 'mod';");
        let imports = refs_of_kind(&file, RefKind::ImportEsm);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn esm_namespace_import_extracts_source() {
        let file = extract_str("import * as ns from 'mod';");
        let imports = refs_of_kind(&file, RefKind::ImportEsm);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn require_call_is_cjs_import() {
        let file = extract_str("const foo = require('foo');");
        let imports = refs_of_kind(&file, RefKind::ImportCjs);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "foo");
    }

    #[test]
    fn class_method_arrow_extracted() {
        let source = r#"
class Foo {
  bar() { return 1; }
}
const baz = () => 2;
"#;
        let file = extract_str(source);
        let classes = nodes_of_kind(&file, NodeKind::Class);
        let methods = nodes_of_kind(&file, NodeKind::Method);
        let arrows = nodes_of_kind(&file, NodeKind::ArrowFunction);
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Foo");
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "bar");
        assert_eq!(arrows.len(), 1);
        assert_eq!(arrows[0].name, "baz");
    }

    #[test]
    fn function_declarations_extracted() {
        let file = extract_str("function alpha(x) { return x; }");
        let funcs = nodes_of_kind(&file, NodeKind::Function);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "alpha");
    }

    #[test]
    fn calls_collected() {
        let file = extract_str("foo(); bar.baz();");
        let calls = refs_of_kind(&file, RefKind::Call);
        let names: Vec<_> = calls.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"baz"));
    }

    #[test]
    fn jsx_component_uppercase_captured() {
        let source = r#"
const tree = <MyComponent prop={x} />;
"#;
        let file = extract_str(source);
        let comps = refs_of_kind(&file, RefKind::JsxComponent);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].name, "MyComponent");
    }

    #[test]
    fn jsx_lowercase_not_component() {
        let source = "const tree = <div>hi</div>;";
        let file = extract_str(source);
        let comps = refs_of_kind(&file, RefKind::JsxComponent);
        assert!(comps.is_empty());
    }

    #[test]
    fn esm_export_named_emits_ref() {
        let file = extract_str("export function foo() {}");
        let exports = refs_of_kind(&file, RefKind::ExportEsm);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "foo");
    }

    #[test]
    fn esm_export_const_extracts_variable_name() {
        let file = extract_str("export const value = 1;");
        let exports = refs_of_kind(&file, RefKind::ExportEsm);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "value");
    }

    #[test]
    fn esm_export_default_anonymous_uses_default_label() {
        let file = extract_str("export default 42;");
        let exports = refs_of_kind(&file, RefKind::ExportEsm);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "default");
    }

    #[test]
    fn arrow_assigned_to_string_is_not_arrow_function() {
        let file = extract_str("const message = \"use function with =>\";");
        let arrows = nodes_of_kind(&file, NodeKind::ArrowFunction);
        let vars = nodes_of_kind(&file, NodeKind::Variable);
        assert!(arrows.is_empty());
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].name, "message");
    }

    #[test]
    fn cjs_module_exports_emits_ref() {
        let file = extract_str("module.exports = function () {};");
        let exports = refs_of_kind(&file, RefKind::ExportCjs);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "module.exports");
    }

    #[test]
    fn parse_error_emits_diagnostic_without_failing() {
        let file = extract_str("function ( {");
        assert!(!file.diagnostics.is_empty());
    }

    #[test]
    fn plugin_metadata() {
        let plugin = JavaScriptPlugin::new();
        assert_eq!(plugin.id(), LanguageId::JavaScript);
        assert!(plugin.extensions().contains(&"jsx"));
    }
}
