use std::path::Path;
use std::sync::{Arc, OnceLock};

use tree_sitter::{Language, Node as TsNode, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::extract::{
    Diagnostic, ExtractedFile, LocalNodeId, LocalRefId, Node, Position, Ref, Severity, Span,
};
use crate::language::{LanguageId, LanguagePlugin, LanguageQueries};

const KIND_CLASS: &str = "class";
const KIND_FUNCTION: &str = "function";
const KIND_METHOD: &str = "method";
const KIND_ARROW_FUNCTION: &str = "arrow_function";
const KIND_VARIABLE: &str = "variable";

const REF_IMPORT_ESM: &str = "import_esm";
const REF_IMPORT_CJS: &str = "import_cjs";
pub(super) const REF_IMPORT_NAMED: &str = "import_named";
pub(super) const REF_IMPORT_DEFAULT: &str = "import_default";
pub(super) const REF_IMPORT_NAMESPACE: &str = "import_namespace";
const REF_EXPORT_ESM: &str = "export_esm";
const REF_EXPORT_CJS: &str = "export_cjs";
const REF_CALL: &str = "call";
const REF_MEMBER_ACCESS: &str = "member_access";
const REF_JSX_COMPONENT: &str = "jsx_component";
const REF_JSX_ELEMENT: &str = "jsx_element";

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

static QUERIES: LanguageQueries = LanguageQueries {
    definitions: DEFINITIONS_QUERY,
    imports: IMPORT_QUERY,
    exports: EXPORT_QUERY,
    types: "",
    routes: "",
};

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

    fn queries(&self) -> &'static LanguageQueries {
        &QUERIES
    }

    fn tree_sitter_language(&self) -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }

    fn extract(&self, source: &[u8], path: &Path) -> ExtractedFile {
        let mut file = ExtractedFile {
            path: path.to_path_buf(),
            ..Default::default()
        };
        let Some(tree) = parse(source) else {
            return file;
        };
        extract_into(&tree, source, &mut file);
        file
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

thread_local! {
    static PARSER: std::cell::RefCell<Option<Parser>> = const { std::cell::RefCell::new(None) };
}

pub fn parse(source: &[u8]) -> Option<Tree> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut p = Parser::new();
            p.set_language(language()).ok()?;
            *slot = Some(p);
        }
        slot.as_mut().unwrap().parse(source, None)
    })
}

pub fn extract(source: &[u8], path: &Path) -> ExtractedFile {
    let mut file = ExtractedFile {
        path: path.to_path_buf(),
        ..Default::default()
    };
    let Some(tree) = parse(source) else {
        return file;
    };
    extract_into(&tree, source, &mut file);
    file
}

fn extract_into(tree: &Tree, source: &[u8], out: &mut ExtractedFile) {
    let root = tree.root_node();
    let mut node_id: LocalNodeId = 0;
    let mut ref_id: LocalRefId = 0;
    let mut container_ranges: Vec<ContainerRange> = Vec::new();
    collect_definitions(
        &root,
        source,
        &mut out.nodes,
        &mut node_id,
        &mut container_ranges,
    );
    collect_imports(&root, source, &mut out.refs, &mut ref_id);
    collect_exports(&root, source, &mut out.refs, &mut ref_id);
    collect_calls_and_jsx(root, source, &mut out.refs, &mut ref_id, &container_ranges);
    collect_diagnostics(root, &mut out.diagnostics);
}

/// `(start_byte, end_byte, local_id)` for each top-level / nested definition,
/// used to attribute refs (calls, member accesses, JSX components, type refs)
/// to their enclosing function/class/method.
pub(super) type ContainerRange = (usize, usize, crate::extract::LocalNodeId);

/// Walk `container_ranges` and return the smallest enclosing definition's
/// local id, or `None` if `(start, end)` is at module top-level. Smallest
/// scope wins ties so a method is picked over its enclosing class. Mirrors
/// PHP's `enclosing_definition_local_id_v2`.
pub(super) fn enclosing_def(
    container_ranges: &[ContainerRange],
    start: usize,
    end: usize,
) -> Option<crate::extract::LocalNodeId> {
    let mut best: Option<(usize, crate::extract::LocalNodeId)> = None;
    for &(c_start, c_end, id) in container_ranges {
        if c_start <= start && c_end >= end && (c_start < start || c_end > end) {
            let span = c_end - c_start;
            if best.as_ref().is_none_or(|(s, _)| span < *s) {
                best = Some((span, id));
            }
        }
    }
    best.map(|(_, id)| id)
}

pub(super) fn span_from_node(node: TsNode<'_>) -> Span {
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

pub(super) fn slice_text(source: &[u8], node: TsNode<'_>) -> String {
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

fn collect_definitions(
    root: &TsNode<'_>,
    source: &[u8],
    out: &mut Vec<Node>,
    next_id: &mut LocalNodeId,
    container_ranges: &mut Vec<ContainerRange>,
) {
    let query = definitions_query();
    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut matches = cursor.matches(query, *root, source);
    while let Some(m) = matches.next() {
        let mut kind: Option<&'static str> = None;
        let mut name: Option<String> = None;
        let mut def_node: Option<TsNode<'_>> = None;
        for cap in m.captures {
            let cname = names[cap.index as usize];
            match cname {
                "class.def" => {
                    kind = Some(KIND_CLASS);
                    def_node = Some(cap.node);
                }
                "function.def" => {
                    kind = Some(KIND_FUNCTION);
                    def_node = Some(cap.node);
                }
                "method.def" => {
                    kind = Some(KIND_METHOD);
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
            let id = *next_id;
            *next_id += 1;
            container_ranges.push((node.start_byte(), node.end_byte(), id));
            out.push(Node {
                id,
                kind: kind.to_owned(),
                qname: name.clone(),
                name,
                span: span_from_node(node),
                parent: enclosing_def(container_ranges, node.start_byte(), node.end_byte()),
            });
        }
    }
}

pub(super) fn variable_kind_from_declarator(declarator: TsNode<'_>) -> &'static str {
    let Some(value) = declarator.child_by_field_name("value") else {
        return KIND_VARIABLE;
    };
    match value.kind() {
        "arrow_function" => KIND_ARROW_FUNCTION,
        "function_expression" | "generator_function" => KIND_FUNCTION,
        "class" => KIND_CLASS,
        _ => KIND_VARIABLE,
    }
}

fn collect_imports(root: &TsNode<'_>, source: &[u8], out: &mut Vec<Ref>, next_id: &mut LocalRefId) {
    let query = import_query();
    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut matches = cursor.matches(query, *root, source);
    while let Some(m) = matches.next() {
        let mut esm_source: Option<TsNode<'_>> = None;
        let mut esm_stmt: Option<TsNode<'_>> = None;
        let mut cjs_fn: Option<TsNode<'_>> = None;
        let mut cjs_source: Option<TsNode<'_>> = None;
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
            let module_name = strip_string_quotes(&raw).to_owned();
            // Module-level ref: drives the file→file `imports` edge.
            let id = *next_id;
            *next_id += 1;
            out.push(Ref {
                id,
                kind: REF_IMPORT_ESM.to_owned(),
                qname: None,
                alias: None,
                name: module_name.clone(),
                span: span_from_node(stmt),
                container: None,
            });
            // Per-binding refs: drive cross-file edges from this file's
            // call/usage sites to the actual exported symbol. The owner's
            // `rewrite_imports` pass rewrites `qname` to a composite
            // `<resolved_path>#<symbol>` key matched in the symbol table.
            emit_named_import_bindings(stmt, source, &module_name, out, next_id);
        }
        if let (Some(func), Some(src)) = (cjs_fn, cjs_source) {
            let fn_name = slice_text(source, func);
            if fn_name == "require" {
                let raw = slice_text(source, src);
                let name = strip_string_quotes(&raw).to_owned();
                let id = *next_id;
                *next_id += 1;
                out.push(Ref {
                    id,
                    kind: REF_IMPORT_CJS.to_owned(),
                    qname: None,
                    alias: None,
                    name,
                    span: span_from_node(src),
                    container: None,
                });
            }
        }
    }
}

/// Walk an `import_statement` node and push one Ref per binding (default,
/// namespace, named). `module_name` is the raw module string from the
/// `from '...'` clause; it is carried through `qname` so the owner's
/// resolver pass can rewrite it to a project-relative path.
pub(super) fn emit_named_import_bindings(
    stmt: TsNode<'_>,
    source: &[u8],
    module_name: &str,
    out: &mut Vec<Ref>,
    next_id: &mut LocalRefId,
) {
    let mut cursor = stmt.walk();
    for child in stmt.named_children(&mut cursor) {
        if child.kind() == "import_clause" {
            walk_import_clause(child, source, module_name, out, next_id);
        }
    }
}

fn walk_import_clause(
    clause: TsNode<'_>,
    source: &[u8],
    module_name: &str,
    out: &mut Vec<Ref>,
    next_id: &mut LocalRefId,
) {
    let mut cursor = clause.walk();
    for child in clause.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                // Default import: `import Foo from 'mod'`
                let local = slice_text(source, child);
                push_binding(
                    out,
                    next_id,
                    REF_IMPORT_DEFAULT,
                    "default",
                    Some(&local),
                    child,
                    module_name,
                );
            }
            "namespace_import" => {
                // `import * as ns from 'mod'`
                let mut ns_cursor = child.walk();
                let alias = child
                    .named_children(&mut ns_cursor)
                    .find(|c| c.kind() == "identifier")
                    .map(|c| slice_text(source, c));
                push_binding(
                    out,
                    next_id,
                    REF_IMPORT_NAMESPACE,
                    "*",
                    alias.as_deref(),
                    child,
                    module_name,
                );
            }
            "named_imports" => {
                let mut spec_cursor = child.walk();
                for spec in child.named_children(&mut spec_cursor) {
                    if spec.kind() == "import_specifier" {
                        push_named_specifier(spec, source, module_name, out, next_id);
                    }
                }
            }
            _ => {}
        }
    }
}

fn push_named_specifier(
    spec: TsNode<'_>,
    source: &[u8],
    module_name: &str,
    out: &mut Vec<Ref>,
    next_id: &mut LocalRefId,
) {
    let Some(name_node) = spec.child_by_field_name("name") else {
        return;
    };
    let symbol = slice_text(source, name_node);
    let alias = spec
        .child_by_field_name("alias")
        .map(|n| slice_text(source, n));
    push_binding(
        out,
        next_id,
        REF_IMPORT_NAMED,
        &symbol,
        alias.as_deref(),
        spec,
        module_name,
    );
}

fn push_binding(
    out: &mut Vec<Ref>,
    next_id: &mut LocalRefId,
    kind: &str,
    symbol: &str,
    alias: Option<&str>,
    span_node: TsNode<'_>,
    module_name: &str,
) {
    let id = *next_id;
    *next_id += 1;
    out.push(Ref {
        id,
        kind: kind.to_owned(),
        // qname carries the module source unchanged out of the extractor;
        // the owner's rewrite pass turns it into `<resolved>#<symbol>`.
        qname: Some(module_name.to_owned()),
        alias: alias.map(str::to_owned),
        name: symbol.to_owned(),
        span: span_from_node(span_node),
        container: None,
    });
}

fn collect_exports(root: &TsNode<'_>, source: &[u8], out: &mut Vec<Ref>, next_id: &mut LocalRefId) {
    let query = export_query();
    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut matches = cursor.matches(query, *root, source);
    while let Some(m) = matches.next() {
        let mut esm_export: Option<TsNode<'_>> = None;
        let mut cjs_obj: Option<TsNode<'_>> = None;
        let mut cjs_prop: Option<TsNode<'_>> = None;
        let mut cjs_stmt: Option<TsNode<'_>> = None;
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
            let id = *next_id;
            *next_id += 1;
            out.push(Ref {
                id,
                kind: REF_EXPORT_ESM.to_owned(),
                qname: None,
                alias: None,
                name,
                span: span_from_node(node),
                container: None,
            });
        }
        if let (Some(obj), Some(prop), Some(stmt)) = (cjs_obj, cjs_prop, cjs_stmt) {
            let obj_text = slice_text(source, obj);
            let prop_text = slice_text(source, prop);
            if obj_text == "module" && prop_text == "exports" {
                let id = *next_id;
                *next_id += 1;
                out.push(Ref {
                    id,
                    kind: REF_EXPORT_CJS.to_owned(),
                    qname: None,
                    alias: None,
                    name: "module.exports".to_owned(),
                    span: span_from_node(stmt),
                    container: None,
                });
            } else if obj_text == "exports" {
                let id = *next_id;
                *next_id += 1;
                out.push(Ref {
                    id,
                    kind: REF_EXPORT_CJS.to_owned(),
                    qname: None,
                    alias: None,
                    name: format!("exports.{prop_text}"),
                    span: span_from_node(stmt),
                    container: None,
                });
            }
        }
    }
}

fn export_label(export_node: TsNode<'_>, source: &[u8]) -> String {
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

pub(super) fn declaration_name(decl: TsNode<'_>, source: &[u8]) -> Option<String> {
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

fn is_default_export(export_node: TsNode<'_>) -> bool {
    let mut cursor = export_node.walk();
    for child in export_node.children(&mut cursor) {
        if !child.is_named() && child.kind() == "default" {
            return true;
        }
    }
    false
}

pub(super) fn collect_calls_and_jsx(
    root: TsNode<'_>,
    source: &[u8],
    out: &mut Vec<Ref>,
    next_id: &mut LocalRefId,
    container_ranges: &[ContainerRange],
) {
    walk_tree(root, |node| {
        visit_call_or_jsx(node, source, out, next_id, container_ranges)
    });
}

fn visit_call_or_jsx(
    node: TsNode<'_>,
    source: &[u8],
    out: &mut Vec<Ref>,
    next_id: &mut LocalRefId,
    container_ranges: &[ContainerRange],
) {
    match node.kind() {
        "call_expression" => {
            if let Some(callee) = node.child_by_field_name("function")
                && !is_require_call(callee, source)
            {
                let name = callee_label(callee, source);
                if !name.is_empty() {
                    let id = *next_id;
                    *next_id += 1;
                    out.push(Ref {
                        id,
                        kind: REF_CALL.to_owned(),
                        qname: None,
                        alias: None,
                        name,
                        span: span_from_node(node),
                        container: enclosing_def(
                            container_ranges,
                            node.start_byte(),
                            node.end_byte(),
                        ),
                    });
                }
            }
        }
        "member_expression" => {
            if let Some(prop) = node.child_by_field_name("property") {
                let name = slice_text(source, prop);
                let id = *next_id;
                *next_id += 1;
                out.push(Ref {
                    id,
                    kind: REF_MEMBER_ACCESS.to_owned(),
                    qname: None,
                    alias: None,
                    name,
                    span: span_from_node(node),
                    container: enclosing_def(container_ranges, node.start_byte(), node.end_byte()),
                });
            }
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = slice_text(source, name_node);
                let kind = if is_component_name(&text) {
                    REF_JSX_COMPONENT
                } else {
                    REF_JSX_ELEMENT
                };
                let id = *next_id;
                *next_id += 1;
                out.push(Ref {
                    id,
                    kind: kind.to_owned(),
                    qname: None,
                    alias: None,
                    name: text,
                    span: span_from_node(node),
                    container: enclosing_def(container_ranges, node.start_byte(), node.end_byte()),
                });
            }
        }
        _ => {}
    }
}

pub(super) fn is_require_call(callee: TsNode<'_>, source: &[u8]) -> bool {
    callee.kind() == "identifier" && slice_text(source, callee) == "require"
}

pub(super) fn callee_label(callee: TsNode<'_>, source: &[u8]) -> String {
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

pub(super) fn collect_diagnostics(root: TsNode<'_>, out: &mut Vec<Diagnostic>) {
    if !root.has_error() {
        return;
    }
    walk_tree(root, |node| {
        if node.is_error() || node.is_missing() {
            out.push(Diagnostic {
                severity: Severity::Error,
                message: if node.is_missing() {
                    format!("missing {}", node.kind())
                } else {
                    "syntax error".to_owned()
                },
                span: Some(span_from_node(node)),
            });
        }
    });
}

pub(super) fn walk_tree<F: FnMut(TsNode<'_>)>(root: TsNode<'_>, mut visit: F) {
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
    use std::path::PathBuf;

    fn extract_str(source: &str) -> ExtractedFile {
        extract(source.as_bytes(), &PathBuf::from("test.js"))
    }

    fn refs_of_kind<'a>(file: &'a ExtractedFile, kind: &str) -> Vec<&'a Ref> {
        file.refs.iter().filter(|r| r.kind == kind).collect()
    }

    fn nodes_of_kind<'a>(file: &'a ExtractedFile, kind: &str) -> Vec<&'a Node> {
        file.nodes.iter().filter(|n| n.kind == kind).collect()
    }

    #[test]
    fn esm_default_import_extracts_source() {
        let file = extract_str("import x from 'mod';");
        let imports = refs_of_kind(&file, REF_IMPORT_ESM);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn esm_named_imports_extract_source() {
        let file = extract_str("import {a, b} from 'mod';");
        let imports = refs_of_kind(&file, REF_IMPORT_ESM);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn esm_namespace_import_extracts_source() {
        let file = extract_str("import * as ns from 'mod';");
        let imports = refs_of_kind(&file, REF_IMPORT_ESM);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "mod");
    }

    #[test]
    fn require_call_is_cjs_import() {
        let file = extract_str("const foo = require('foo');");
        let imports = refs_of_kind(&file, REF_IMPORT_CJS);
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
        let classes = nodes_of_kind(&file, KIND_CLASS);
        let methods = nodes_of_kind(&file, KIND_METHOD);
        let arrows = nodes_of_kind(&file, KIND_ARROW_FUNCTION);
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
        let funcs = nodes_of_kind(&file, KIND_FUNCTION);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "alpha");
    }

    #[test]
    fn calls_collected() {
        let file = extract_str("foo(); bar.baz();");
        let calls = refs_of_kind(&file, REF_CALL);
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
        let comps = refs_of_kind(&file, REF_JSX_COMPONENT);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].name, "MyComponent");
    }

    #[test]
    fn jsx_lowercase_not_component() {
        let source = "const tree = <div>hi</div>;";
        let file = extract_str(source);
        let comps = refs_of_kind(&file, REF_JSX_COMPONENT);
        assert!(comps.is_empty());
    }

    #[test]
    fn esm_export_named_emits_ref() {
        let file = extract_str("export function foo() {}");
        let exports = refs_of_kind(&file, REF_EXPORT_ESM);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "foo");
    }

    #[test]
    fn esm_export_const_extracts_variable_name() {
        let file = extract_str("export const value = 1;");
        let exports = refs_of_kind(&file, REF_EXPORT_ESM);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "value");
    }

    #[test]
    fn esm_export_default_anonymous_uses_default_label() {
        let file = extract_str("export default 42;");
        let exports = refs_of_kind(&file, REF_EXPORT_ESM);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "default");
    }

    #[test]
    fn arrow_assigned_to_string_is_not_arrow_function() {
        let file = extract_str("const message = \"use function with =>\";");
        let arrows = nodes_of_kind(&file, KIND_ARROW_FUNCTION);
        let vars = nodes_of_kind(&file, KIND_VARIABLE);
        assert!(arrows.is_empty());
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].name, "message");
    }

    #[test]
    fn cjs_module_exports_emits_ref() {
        let file = extract_str("module.exports = function () {};");
        let exports = refs_of_kind(&file, REF_EXPORT_CJS);
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
