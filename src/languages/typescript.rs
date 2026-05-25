use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::extract::{
    Diagnostic as CanonicalDiagnostic, ExtractedFile as CanonicalExtractedFile,
    Node as CanonicalNode, Position, Ref as CanonicalRef, Severity, Span as CanonicalSpan,
};
use crate::language::{
    LanguageId as CanonicalLanguageId, LanguagePlugin as CanonicalLanguagePlugin, LanguageQueries,
};

use super::javascript::{
    Diagnostic, DiagnosticSeverity, ExtractedFile, ExtractedNode, NodeKind, Ref, RefKind, Span,
    callee_label, collect_diagnostics, declaration_name, is_component_name, is_require_call,
    slice_text, strip_string_quotes, variable_kind_from_declarator, walk_tree,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TsFlavor {
    TypeScript,
    Tsx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TsNodeKind {
    Interface,
    TypeAlias,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TsRefKind {
    TypeReference,
}

const DEFINITIONS_QUERY: &str = r#"
(class_declaration
  name: (type_identifier) @class.name) @class.def

(abstract_class_declaration
  name: (type_identifier) @class.name) @class.def

(function_declaration
  name: (identifier) @function.name) @function.def

(generator_function_declaration
  name: (identifier) @function.name) @function.def

(method_definition
  name: (property_identifier) @method.name) @method.def

(variable_declarator
  name: (identifier) @var.name) @var.def

(interface_declaration
  name: (type_identifier) @interface.name) @interface.def

(type_alias_declaration
  name: (type_identifier) @type.name) @type.def
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

pub struct TypeScriptPlugin {
    flavor: TsFlavor,
}

impl TypeScriptPlugin {
    pub const fn typescript() -> Self {
        Self {
            flavor: TsFlavor::TypeScript,
        }
    }

    pub const fn tsx() -> Self {
        Self {
            flavor: TsFlavor::Tsx,
        }
    }

    pub const fn flavor(&self) -> TsFlavor {
        self.flavor
    }
}

impl CanonicalLanguagePlugin for TypeScriptPlugin {
    fn id(&self) -> CanonicalLanguageId {
        match self.flavor {
            TsFlavor::TypeScript => CanonicalLanguageId::TypeScript,
            TsFlavor::Tsx => CanonicalLanguageId::Tsx,
        }
    }

    fn extensions(&self) -> &'static [&'static str] {
        match self.flavor {
            TsFlavor::TypeScript => &["ts", "mts", "cts"],
            TsFlavor::Tsx => &["tsx"],
        }
    }

    fn queries(&self) -> &'static LanguageQueries {
        match self.flavor {
            TsFlavor::TypeScript => &TYPESCRIPT_QUERIES,
            TsFlavor::Tsx => &TSX_QUERIES,
        }
    }

    fn tree_sitter_language(&self) -> Language {
        match self.flavor {
            TsFlavor::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            TsFlavor::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    fn extract(&self, source: &[u8], path: &Path) -> CanonicalExtractedFile {
        let legacy = extract(self.flavor, source);
        to_canonical(path.to_path_buf(), legacy)
    }
}

static TYPESCRIPT_QUERIES: LanguageQueries = LanguageQueries {
    definitions: DEFINITIONS_QUERY,
    imports: IMPORT_QUERY,
    exports: EXPORT_QUERY,
    types: "",
    routes: "",
};

static TSX_QUERIES: LanguageQueries = LanguageQueries {
    definitions: DEFINITIONS_QUERY,
    imports: IMPORT_QUERY,
    exports: EXPORT_QUERY,
    types: "",
    routes: "",
};

fn to_canonical_span(span: Span) -> CanonicalSpan {
    CanonicalSpan {
        start: Position {
            byte: span.start_byte,
            row: span.start_row,
            column: span.start_col,
        },
        end: Position {
            byte: span.end_byte,
            row: span.end_row,
            column: span.end_col,
        },
    }
}

fn node_kind_to_str(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Module => "module",
        NodeKind::Class => "class",
        NodeKind::Function => "function",
        NodeKind::Method => "method",
        NodeKind::ArrowFunction => "arrow_function",
        NodeKind::Variable => "variable",
    }
}

fn ts_node_kind_to_str(kind: TsNodeKind) -> &'static str {
    match kind {
        TsNodeKind::Interface => "interface",
        TsNodeKind::TypeAlias => "type_alias",
    }
}

fn ref_kind_to_str(kind: RefKind) -> &'static str {
    match kind {
        RefKind::ImportEsm => "import_esm",
        RefKind::ImportCjs => "import_cjs",
        RefKind::ExportEsm => "export_esm",
        RefKind::ExportCjs => "export_cjs",
        RefKind::Call => "call",
        RefKind::MemberAccess => "member_access",
        RefKind::JsxComponent => "jsx_component",
    }
}

fn diagnostic_severity_to_canonical(severity: DiagnosticSeverity) -> Severity {
    match severity {
        DiagnosticSeverity::Error => Severity::Error,
        DiagnosticSeverity::Warning => Severity::Warning,
    }
}

fn to_canonical(path: PathBuf, legacy: TsExtractedFile) -> CanonicalExtractedFile {
    let mut nodes: Vec<CanonicalNode> =
        Vec::with_capacity(legacy.base.nodes.len() + legacy.type_nodes.len());
    let mut next_node_id: u32 = 0;

    for node in legacy.base.nodes {
        nodes.push(CanonicalNode {
            id: next_node_id,
            kind: node_kind_to_str(node.kind).to_owned(),
            name: node.name.clone(),
            qname: node.name,
            span: to_canonical_span(node.span),
            parent: None,
        });
        next_node_id += 1;
    }

    for node in legacy.type_nodes {
        nodes.push(CanonicalNode {
            id: next_node_id,
            kind: ts_node_kind_to_str(node.kind).to_owned(),
            name: node.name.clone(),
            qname: node.name,
            span: to_canonical_span(node.span),
            parent: None,
        });
        next_node_id += 1;
    }

    let mut refs: Vec<CanonicalRef> =
        Vec::with_capacity(legacy.base.refs.len() + legacy.type_refs.len());
    let mut next_ref_id: u32 = 0;

    for r in legacy.base.refs {
        refs.push(CanonicalRef {
            id: next_ref_id,
            kind: ref_kind_to_str(r.kind).to_owned(),
            name: r.name,
            qname: None,
            alias: None,
            span: to_canonical_span(r.span),
            container: None,
        });
        next_ref_id += 1;
    }

    for r in legacy.type_refs {
        let kind_str = match r.kind {
            TsRefKind::TypeReference => "type_reference",
        };
        refs.push(CanonicalRef {
            id: next_ref_id,
            kind: kind_str.to_owned(),
            name: r.name,
            qname: None,
            alias: None,
            span: to_canonical_span(r.span),
            container: None,
        });
        next_ref_id += 1;
    }

    let diagnostics = legacy
        .base
        .diagnostics
        .into_iter()
        .map(|d| CanonicalDiagnostic {
            severity: diagnostic_severity_to_canonical(d.severity),
            message: d.message,
            span: Some(to_canonical_span(d.span)),
        })
        .collect();

    CanonicalExtractedFile {
        path,
        nodes,
        refs,
        diagnostics,
    }
}

fn language_id_for(flavor: TsFlavor) -> super::javascript::LanguageId {
    match flavor {
        TsFlavor::TypeScript => super::javascript::LanguageId::TypeScript,
        TsFlavor::Tsx => super::javascript::LanguageId::Tsx,
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TsExtractedFile {
    pub base: ExtractedFile,
    pub type_nodes: Vec<TsExtractedNode>,
    pub type_refs: Vec<TsRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsExtractedNode {
    pub kind: TsNodeKind,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsRef {
    pub kind: TsRefKind,
    pub name: String,
    pub span: Span,
}

fn typescript_language() -> &'static Language {
    static LANG: OnceLock<Language> = OnceLock::new();
    LANG.get_or_init(|| tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
}

fn tsx_language() -> &'static Language {
    static LANG: OnceLock<Language> = OnceLock::new();
    LANG.get_or_init(|| tree_sitter_typescript::LANGUAGE_TSX.into())
}

fn language_for(flavor: TsFlavor) -> &'static Language {
    match flavor {
        TsFlavor::TypeScript => typescript_language(),
        TsFlavor::Tsx => tsx_language(),
    }
}

fn definitions_query(flavor: TsFlavor) -> &'static Arc<Query> {
    static TS_QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    static TSX_QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    let cell = match flavor {
        TsFlavor::TypeScript => &TS_QUERY,
        TsFlavor::Tsx => &TSX_QUERY,
    };
    cell.get_or_init(|| {
        Arc::new(
            Query::new(language_for(flavor), DEFINITIONS_QUERY)
                .unwrap_or_else(|err| panic!("invalid typescript definitions query: {err:?}")),
        )
    })
}

fn import_query(flavor: TsFlavor) -> &'static Arc<Query> {
    static TS_QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    static TSX_QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    let cell = match flavor {
        TsFlavor::TypeScript => &TS_QUERY,
        TsFlavor::Tsx => &TSX_QUERY,
    };
    cell.get_or_init(|| {
        Arc::new(
            Query::new(language_for(flavor), IMPORT_QUERY)
                .unwrap_or_else(|err| panic!("invalid typescript import query: {err:?}")),
        )
    })
}

fn export_query(flavor: TsFlavor) -> &'static Arc<Query> {
    static TS_QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    static TSX_QUERY: OnceLock<Arc<Query>> = OnceLock::new();
    let cell = match flavor {
        TsFlavor::TypeScript => &TS_QUERY,
        TsFlavor::Tsx => &TSX_QUERY,
    };
    cell.get_or_init(|| {
        Arc::new(
            Query::new(language_for(flavor), EXPORT_QUERY)
                .unwrap_or_else(|err| panic!("invalid typescript export query: {err:?}")),
        )
    })
}

pub fn parse(flavor: TsFlavor, source: &[u8]) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language_for(flavor)).ok()?;
    parser.parse(source, None)
}

pub fn extract(flavor: TsFlavor, source: &[u8]) -> TsExtractedFile {
    let Some(tree) = parse(flavor, source) else {
        let mut empty = TsExtractedFile::default();
        empty.base.language = Some(language_id_for(flavor));
        return empty;
    };
    extract_internal(flavor, &tree, source)
}

fn extract_internal(flavor: TsFlavor, tree: &Tree, source: &[u8]) -> TsExtractedFile {
    let mut file = TsExtractedFile::default();
    file.base.language = Some(language_id_for(flavor));
    let root = tree.root_node();
    collect_definitions(
        flavor,
        &root,
        source,
        &mut file.base.nodes,
        &mut file.type_nodes,
    );
    collect_imports(flavor, &root, source, &mut file.base.refs);
    collect_exports(flavor, &root, source, &mut file.base.refs);
    collect_calls_and_jsx(flavor, root, source, &mut file.base.refs);
    collect_type_references(root, source, &mut file.type_refs);
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    collect_diagnostics(root, &mut diagnostics);
    file.base.diagnostics = diagnostics;
    file
}

fn collect_definitions(
    flavor: TsFlavor,
    root: &Node<'_>,
    source: &[u8],
    js_out: &mut Vec<ExtractedNode>,
    ts_out: &mut Vec<TsExtractedNode>,
) {
    let query = definitions_query(flavor);
    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut matches = cursor.matches(query, *root, source);
    while let Some(m) = matches.next() {
        let mut js_kind: Option<NodeKind> = None;
        let mut ts_kind: Option<TsNodeKind> = None;
        let mut name: Option<String> = None;
        let mut def_node: Option<Node<'_>> = None;
        for cap in m.captures {
            match names[cap.index as usize] {
                "class.def" => {
                    js_kind = Some(NodeKind::Class);
                    def_node = Some(cap.node);
                }
                "function.def" => {
                    js_kind = Some(NodeKind::Function);
                    def_node = Some(cap.node);
                }
                "method.def" => {
                    js_kind = Some(NodeKind::Method);
                    def_node = Some(cap.node);
                }
                "var.def" => {
                    js_kind = Some(variable_kind_from_declarator(cap.node));
                    def_node = Some(cap.node);
                }
                "interface.def" => {
                    ts_kind = Some(TsNodeKind::Interface);
                    def_node = Some(cap.node);
                }
                "type.def" => {
                    ts_kind = Some(TsNodeKind::TypeAlias);
                    def_node = Some(cap.node);
                }
                "class.name" | "function.name" | "method.name" | "var.name" | "interface.name"
                | "type.name" => {
                    name = Some(slice_text(source, cap.node));
                }
                _ => {}
            }
        }
        let Some(name) = name else { continue };
        let Some(node) = def_node else { continue };
        let span = Span::from_node(node);
        if let Some(kind) = ts_kind {
            ts_out.push(TsExtractedNode { kind, name, span });
        } else if let Some(kind) = js_kind {
            js_out.push(ExtractedNode { kind, name, span });
        }
    }
}

fn collect_imports(flavor: TsFlavor, root: &Node<'_>, source: &[u8], out: &mut Vec<Ref>) {
    let query = import_query(flavor);
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

fn collect_exports(flavor: TsFlavor, root: &Node<'_>, source: &[u8], out: &mut Vec<Ref>) {
    let query = export_query(flavor);
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
    let mut cursor = export_node.walk();
    for child in export_node.children(&mut cursor) {
        if !child.is_named() && child.kind() == "default" {
            return "default".to_owned();
        }
    }
    "export".to_owned()
}

fn collect_calls_and_jsx(flavor: TsFlavor, root: Node<'_>, source: &[u8], out: &mut Vec<Ref>) {
    walk_tree(root, |node| match node.kind() {
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
                out.push(Ref {
                    kind: RefKind::MemberAccess,
                    name: slice_text(source, prop),
                    span: Span::from_node(node),
                });
            }
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if matches!(flavor, TsFlavor::Tsx)
                && let Some(name_node) = node.child_by_field_name("name")
            {
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
    });
}

fn collect_type_references(root: Node<'_>, source: &[u8], out: &mut Vec<TsRef>) {
    walk_tree(root, |node| {
        if node.kind() == "type_identifier" && !is_definition_name(node) {
            out.push(TsRef {
                kind: TsRefKind::TypeReference,
                name: slice_text(source, node),
                span: Span::from_node(node),
            });
        }
    });
}

fn is_definition_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "interface_declaration"
        | "type_alias_declaration"
        | "class_declaration"
        | "abstract_class_declaration"
        | "type_parameter" => {
            if let Some(name_field) = parent.child_by_field_name("name") {
                return name_field == node;
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ts_path() -> PathBuf {
        PathBuf::from("module.ts")
    }

    fn tsx_path() -> PathBuf {
        PathBuf::from("Component.tsx")
    }

    fn extract_ts(source: &str) -> CanonicalExtractedFile {
        TypeScriptPlugin::typescript().extract(source.as_bytes(), &ts_path())
    }

    fn extract_tsx(source: &str) -> CanonicalExtractedFile {
        TypeScriptPlugin::tsx().extract(source.as_bytes(), &tsx_path())
    }

    fn refs_of_kind<'a>(file: &'a CanonicalExtractedFile, kind: &str) -> Vec<&'a CanonicalRef> {
        file.refs.iter().filter(|r| r.kind == kind).collect()
    }

    fn nodes_of_kind<'a>(file: &'a CanonicalExtractedFile, kind: &str) -> Vec<&'a CanonicalNode> {
        file.nodes.iter().filter(|n| n.kind == kind).collect()
    }

    #[test]
    fn esm_default_import_extracts_source() {
        let file = extract_ts("import x from 'mod';");
        let imports = refs_of_kind(&file, "import_esm");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn esm_named_imports_extract_source() {
        let file = extract_ts("import {a, b} from 'mod';");
        let imports = refs_of_kind(&file, "import_esm");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn esm_namespace_import_extracts_source() {
        let file = extract_ts("import * as ns from 'mod';");
        let imports = refs_of_kind(&file, "import_esm");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn interface_and_type_alias_extracted() {
        let source = r#"
interface Foo { value: number }
type Bar = string;
"#;
        let file = extract_ts(source);
        let interfaces = nodes_of_kind(&file, "interface");
        let aliases = nodes_of_kind(&file, "type_alias");
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].name, "Foo");
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].name, "Bar");
    }

    #[test]
    fn generic_args_captured_as_type_refs() {
        let source = r#"
type X = Array<Foo>;
"#;
        let file = extract_ts(source);
        let names: Vec<&str> = refs_of_kind(&file, "type_reference")
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert!(names.contains(&"Foo"));
    }

    #[test]
    fn type_annotation_captured_as_type_ref() {
        let source = r#"
function take(x: Person): void {}
"#;
        let file = extract_ts(source);
        let names: Vec<&str> = refs_of_kind(&file, "type_reference")
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert!(names.contains(&"Person"));
    }

    #[test]
    fn class_method_arrow_extracted() {
        let source = r#"
class Foo {
  bar(): number { return 1; }
}
const baz = (): number => 2;
"#;
        let file = extract_ts(source);
        let classes = nodes_of_kind(&file, "class");
        let methods = nodes_of_kind(&file, "method");
        let arrows = nodes_of_kind(&file, "arrow_function");
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Foo");
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "bar");
        assert_eq!(arrows.len(), 1);
        assert_eq!(arrows[0].name, "baz");
    }

    #[test]
    fn require_call_is_cjs_import() {
        let file = extract_ts("const foo = require('foo');");
        let imports = refs_of_kind(&file, "import_cjs");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "foo");
    }

    #[test]
    fn tsx_component_uppercase_captured() {
        let source = r#"
function App() {
  return <MyComponent prop={1} />;
}
"#;
        let file = extract_tsx(source);
        let comps = refs_of_kind(&file, "jsx_component");
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].name, "MyComponent");
    }

    #[test]
    fn tsx_lowercase_not_component() {
        let source = r#"
function App() {
  return <div>hi</div>;
}
"#;
        let file = extract_tsx(source);
        let comps = refs_of_kind(&file, "jsx_component");
        assert!(comps.is_empty());
    }

    #[test]
    fn parse_error_emits_diagnostic_without_failing() {
        let file = extract_ts("function ( {");
        assert!(!file.diagnostics.is_empty());
    }

    #[test]
    fn plugin_metadata() {
        let ts = TypeScriptPlugin::typescript();
        let tsx = TypeScriptPlugin::tsx();
        assert_eq!(ts.id(), CanonicalLanguageId::TypeScript);
        assert_eq!(tsx.id(), CanonicalLanguageId::Tsx);
        assert_eq!(ts.extensions(), &["ts", "mts", "cts"]);
        assert_eq!(tsx.extensions(), &["tsx"]);
        assert_eq!(ts.flavor(), TsFlavor::TypeScript);
        assert_eq!(tsx.flavor(), TsFlavor::Tsx);
    }

    #[test]
    fn plugin_queries_are_static() {
        let ts = TypeScriptPlugin::typescript();
        let tsx = TypeScriptPlugin::tsx();
        let ts_queries = ts.queries();
        let tsx_queries = tsx.queries();
        assert!(!ts_queries.definitions.is_empty());
        assert!(!ts_queries.imports.is_empty());
        assert!(!ts_queries.exports.is_empty());
        assert!(!tsx_queries.definitions.is_empty());
    }

    #[test]
    fn interface_name_not_emitted_as_type_ref() {
        let source = "interface Foo { value: number }";
        let file = extract_ts(source);
        let names: Vec<&str> = refs_of_kind(&file, "type_reference")
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert!(!names.contains(&"Foo"));
    }

    #[test]
    fn generic_argument_emitted_once_per_occurrence() {
        let source = "type X = Array<Foo>; type Y = Map<Bar, Baz>;";
        let file = extract_ts(source);
        let foo_count = refs_of_kind(&file, "type_reference")
            .iter()
            .filter(|r| r.name == "Foo")
            .count();
        let bar_count = refs_of_kind(&file, "type_reference")
            .iter()
            .filter(|r| r.name == "Bar")
            .count();
        let baz_count = refs_of_kind(&file, "type_reference")
            .iter()
            .filter(|r| r.name == "Baz")
            .count();
        assert_eq!(foo_count, 1);
        assert_eq!(bar_count, 1);
        assert_eq!(baz_count, 1);
    }

    #[test]
    fn extracted_file_carries_path() {
        let plugin = TypeScriptPlugin::typescript();
        let path = PathBuf::from("src/example.ts");
        let file = plugin.extract(b"type X = number;", &path);
        assert_eq!(file.path, path);
    }

    #[test]
    fn span_uses_nested_positions() {
        let file = extract_ts("type X = number;");
        let alias = nodes_of_kind(&file, "type_alias");
        assert_eq!(alias.len(), 1);
        let span = alias[0].span;
        assert_eq!(span.start.row, 0);
        assert_eq!(span.start.column, 0);
        assert!(span.end.byte > span.start.byte);
    }

    #[test]
    fn node_ids_are_unique() {
        let source = r#"
class Foo {
  bar(): number { return 1; }
}
interface Iface {}
"#;
        let file = extract_ts(source);
        let mut ids: Vec<u32> = file.nodes.iter().map(|n| n.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), file.nodes.len());
    }
}
