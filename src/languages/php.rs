//! PHP source extractor built on `tree-sitter-php`.
//!
//! Produces an `ExtractedFile` of definitions, imports, and references for a
//! single PHP translation unit. Definition discovery uses small precise
//! Tree-sitter queries; call-site discovery uses manual cursor traversal so
//! we can attribute callees to their enclosing definition without paying for
//! a broad "match everything" query.

use std::cell::RefCell;
use std::path::Path;
use std::sync::OnceLock;

use tree_sitter::{
    Language, Node as TsNode, Parser, Query, QueryCursor, StreamingIterator, Tree, TreeCursor,
};

use crate::extract::{
    Diagnostic, ExtractedFile, LocalNodeId, LocalRefId, Node, Position, Ref, Severity, Span,
};
use crate::language::{LanguageId, LanguagePlugin, LanguageQueries};

const DEFINITIONS_QUERY_SOURCE: &str = include_str!("php_queries/definitions.scm");
const IMPORTS_QUERY_SOURCE: &str = include_str!("php_queries/imports.scm");

static PHP_QUERIES: LanguageQueries = LanguageQueries {
    definitions: DEFINITIONS_QUERY_SOURCE,
    imports: IMPORTS_QUERY_SOURCE,
    exports: "",
    types: "",
    routes: "",
};

fn span_from_node(node: TsNode<'_>) -> Span {
    let start_pos = node.start_position();
    let end_pos = node.end_position();
    let range = node.byte_range();
    Span {
        start: Position {
            byte: range.start,
            row: start_pos.row,
            column: start_pos.column,
        },
        end: Position {
            byte: range.end,
            row: end_pos.row,
            column: end_pos.column,
        },
    }
}

pub struct PhpPlugin;

impl PhpPlugin {
    pub const EXTENSIONS: &'static [&'static str] = &["php"];

    pub fn new() -> Self {
        Self
    }
}

impl Default for PhpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguagePlugin for PhpPlugin {
    fn id(&self) -> LanguageId {
        LanguageId::Php
    }

    fn extensions(&self) -> &'static [&'static str] {
        Self::EXTENSIONS
    }

    fn queries(&self) -> &'static LanguageQueries {
        &PHP_QUERIES
    }

    fn tree_sitter_language(&self) -> Language {
        tree_sitter_php::LANGUAGE_PHP.into()
    }

    fn extract(&self, source: &[u8], path: &Path) -> ExtractedFile {
        let Some(tree) = parse(source) else {
            return file_with_diagnostic(path, Severity::Error, "PHP parser returned no tree");
        };

        let mut extractor = Extractor::new(source);
        extractor.run(&tree);
        extractor.into_file(path)
    }
}

fn language() -> &'static Language {
    static LANG: OnceLock<Language> = OnceLock::new();
    LANG.get_or_init(|| tree_sitter_php::LANGUAGE_PHP.into())
}

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

/// Parse a PHP source with a thread-local parser that's reused across calls.
fn parse(source: &[u8]) -> Option<Tree> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            parser.set_language(language()).ok()?;
            *slot = Some(parser);
        }
        slot.as_mut().unwrap().parse(source, None)
    })
}

fn file_with_diagnostic(path: &Path, severity: Severity, message: &str) -> ExtractedFile {
    ExtractedFile {
        path: path.to_path_buf(),
        nodes: Vec::new(),
        refs: Vec::new(),
        diagnostics: vec![Diagnostic {
            severity,
            message: message.to_string(),
            span: None,
        }],
    }
}

struct CompiledQueries {
    definitions: Query,
    imports: Query,
}

fn compiled_queries() -> &'static CompiledQueries {
    static QUERIES: OnceLock<CompiledQueries> = OnceLock::new();
    QUERIES.get_or_init(|| {
        let language: Language = tree_sitter_php::LANGUAGE_PHP.into();
        let definitions = Query::new(&language, DEFINITIONS_QUERY_SOURCE)
            .unwrap_or_else(|err| panic!("PHP definitions query failed to compile: {err}"));
        let imports = Query::new(&language, IMPORTS_QUERY_SOURCE)
            .unwrap_or_else(|err| panic!("PHP imports query failed to compile: {err}"));
        CompiledQueries {
            definitions,
            imports,
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefinitionCapture {
    Namespace,
    Class,
    Interface,
    Trait,
    Enum,
    EnumCase,
    Function,
    Method,
    Property,
    Constant,
    Extends,
    Implements,
    TraitUse,
}

impl DefinitionCapture {
    fn from_capture_name(name: &str) -> Option<Self> {
        match name {
            "namespace" => Some(Self::Namespace),
            "class" => Some(Self::Class),
            "interface" => Some(Self::Interface),
            "trait" => Some(Self::Trait),
            "enum" => Some(Self::Enum),
            "enum_case" => Some(Self::EnumCase),
            "function" => Some(Self::Function),
            "method" => Some(Self::Method),
            "property" => Some(Self::Property),
            "constant" => Some(Self::Constant),
            "extends" => Some(Self::Extends),
            "implements" => Some(Self::Implements),
            "trait_use" => Some(Self::TraitUse),
            _ => None,
        }
    }

    fn as_node_kind(self) -> Option<&'static str> {
        match self {
            Self::Namespace => Some("namespace"),
            Self::Class => Some("class"),
            Self::Interface => Some("interface"),
            Self::Trait => Some("trait"),
            Self::Enum => Some("enum"),
            Self::EnumCase => Some("enum_case"),
            Self::Function => Some("function"),
            Self::Method => Some("method"),
            Self::Property => Some("property"),
            Self::Constant => Some("constant"),
            Self::Extends | Self::Implements | Self::TraitUse => None,
        }
    }

    fn as_ref_kind(self) -> Option<&'static str> {
        match self {
            Self::Extends => Some("extends"),
            Self::Implements => Some("implements"),
            Self::TraitUse => Some("trait_use"),
            _ => None,
        }
    }
}

struct PendingDefinition {
    kind: &'static str,
    name: String,
    container_node_id: usize,
    span: Span,
}

struct PendingClassRef {
    kind: &'static str,
    name: String,
    span: Span,
}

struct Extractor<'src> {
    source: &'src [u8],
    nodes: Vec<Node>,
    refs: Vec<Ref>,
    diagnostics: Vec<Diagnostic>,
    /// Maps a tree-sitter node id to the local id of the definition produced
    /// for it. Used so a ref capture or call site can attribute itself to the
    /// enclosing class/function.
    container_index: std::collections::HashMap<usize, LocalNodeId>,
}

impl<'src> Extractor<'src> {
    fn new(source: &'src [u8]) -> Self {
        Self {
            source,
            nodes: Vec::new(),
            refs: Vec::new(),
            diagnostics: Vec::new(),
            container_index: std::collections::HashMap::new(),
        }
    }

    fn into_file(self, path: &Path) -> ExtractedFile {
        ExtractedFile {
            path: path.to_path_buf(),
            nodes: self.nodes,
            refs: self.refs,
            diagnostics: self.diagnostics,
        }
    }

    fn run(&mut self, tree: &Tree) {
        let root = tree.root_node();
        self.collect_definitions(root);
        self.collect_imports(root);
        self.collect_calls(root);
        self.collect_syntax_diagnostics(root);
    }

    fn collect_definitions(&mut self, root: TsNode<'_>) {
        let query = &compiled_queries().definitions;

        let name_capture_index = query.capture_index_for_name("name");

        let mut pending_definitions: Vec<PendingDefinition> = Vec::new();
        let mut pending_refs: Vec<PendingClassRef> = Vec::new();

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, root, self.source);

        while let Some(m) = matches.next() {
            let mut capture: Option<(DefinitionCapture, TsNode<'_>)> = None;
            let mut name_node: Option<TsNode<'_>> = None;

            for cap in m.captures.iter() {
                let capture_name = match query.capture_names().get(cap.index as usize) {
                    Some(name) => *name,
                    None => continue,
                };

                if let Some(idx) = name_capture_index
                    && cap.index == idx
                {
                    name_node = Some(cap.node);
                    continue;
                }

                if let Some(kind) = DefinitionCapture::from_capture_name(capture_name) {
                    capture = Some((kind, cap.node));
                }
            }

            let (capture, node) = match capture {
                Some(pair) => pair,
                None => continue,
            };

            let Some(name_node) = name_node else {
                self.diagnostics.push(Diagnostic {
                    severity: Severity::Warning,
                    message: format!(
                        "PHP definition capture without name: kind={capture:?}, node={}",
                        node.kind()
                    ),
                    span: Some(span_from_node(node)),
                });
                continue;
            };

            let name = self.slice_text(name_node).to_string();

            if let Some(kind) = capture.as_node_kind() {
                pending_definitions.push(PendingDefinition {
                    kind,
                    name,
                    container_node_id: node.id(),
                    span: span_from_node(node),
                });
            } else if let Some(kind) = capture.as_ref_kind() {
                pending_refs.push(PendingClassRef {
                    kind,
                    name,
                    span: span_from_node(node),
                });
            }
        }

        pending_definitions.sort_by_key(|p| p.span.start.byte);

        for pending in pending_definitions {
            self.emit_definition(pending, root);
        }

        for pending in pending_refs {
            self.emit_class_ref(pending, root);
        }
    }

    fn emit_definition(&mut self, pending: PendingDefinition, root: TsNode<'_>) {
        let namespace = if pending.kind == "namespace" {
            None
        } else {
            enclosing_namespace(root, pending.span.start.byte, self.source)
        };
        let parent_local_id = enclosing_definition_local_id(
            root,
            pending.span.start.byte,
            pending.span.end.byte,
            &self.container_index,
        );

        let qname = build_qname(
            namespace.as_deref(),
            parent_local_id.and_then(|id| self.nodes.get(id as usize).map(|n| n.qname.as_str())),
            &pending.name,
            pending.kind,
        );

        let local_id = self.nodes.len() as LocalNodeId;
        let node = Node {
            id: local_id,
            kind: pending.kind.to_string(),
            name: pending.name,
            qname,
            span: pending.span,
            parent: parent_local_id,
        };

        self.container_index
            .insert(pending.container_node_id, local_id);
        self.nodes.push(node);
    }

    fn emit_class_ref(&mut self, pending: PendingClassRef, root: TsNode<'_>) {
        let container = enclosing_definition_local_id(
            root,
            pending.span.start.byte,
            pending.span.end.byte,
            &self.container_index,
        );

        let id = self.refs.len() as LocalRefId;
        self.refs.push(Ref {
            id,
            kind: pending.kind.to_string(),
            qname: Some(normalize_qualified_name(&pending.name)),
            name: pending.name,
            alias: None,
            span: pending.span,
            container,
        });
    }

    fn collect_imports(&mut self, root: TsNode<'_>) {
        let query = &compiled_queries().imports;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, root, self.source);

        while let Some(m) = matches.next() {
            for cap in m.captures.iter() {
                let capture_name = match query.capture_names().get(cap.index as usize) {
                    Some(name) => *name,
                    None => continue,
                };
                if capture_name != "import_declaration" {
                    continue;
                }
                self.emit_imports_for_declaration(cap.node, root);
            }
        }
    }

    fn emit_imports_for_declaration(&mut self, decl: TsNode<'_>, root: TsNode<'_>) {
        let mut prefix: Option<String> = None;
        let mut walker = decl.walk();
        for child in decl.named_children(&mut walker) {
            match child.kind() {
                "namespace_name" => {
                    prefix = Some(self.slice_text(child).to_string());
                }
                "namespace_use_clause" => {
                    self.emit_use_clause(child, prefix.as_deref(), root);
                }
                "namespace_use_group" => {
                    let mut group_walker = child.walk();
                    for clause in child.named_children(&mut group_walker) {
                        if clause.kind() == "namespace_use_clause" {
                            self.emit_use_clause(clause, prefix.as_deref(), root);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn emit_use_clause(&mut self, clause: TsNode<'_>, prefix: Option<&str>, root: TsNode<'_>) {
        let mut walker = clause.walk();
        let mut name_node: Option<TsNode<'_>> = None;
        for child in clause.named_children(&mut walker) {
            if matches!(child.kind(), "name" | "qualified_name") && name_node.is_none() {
                name_node = Some(child);
            }
        }
        let Some(name_node) = name_node else {
            self.diagnostics.push(Diagnostic {
                severity: Severity::Warning,
                message: "PHP use clause missing imported name".to_string(),
                span: Some(span_from_node(clause)),
            });
            return;
        };
        let alias = clause
            .child_by_field_name("alias")
            .map(|n| self.slice_text(n).to_string());

        let raw_name = self.slice_text(name_node).to_string();
        let qname = if let Some(prefix) = prefix {
            let prefix_normalized = prefix.trim_start_matches('\\');
            let local = raw_name.trim_start_matches('\\');
            format!("{prefix_normalized}\\{local}")
        } else {
            normalize_qualified_name(&raw_name)
        };

        let display_name = alias
            .clone()
            .unwrap_or_else(|| last_segment(&qname).to_string());

        let container = enclosing_definition_local_id(
            root,
            clause.start_byte(),
            clause.end_byte(),
            &self.container_index,
        );

        let id = self.refs.len() as LocalRefId;
        self.refs.push(Ref {
            id,
            kind: "import".to_string(),
            name: display_name,
            qname: Some(qname),
            alias,
            span: span_from_node(clause),
            container,
        });
    }

    fn collect_calls(&mut self, root: TsNode<'_>) {
        let mut cursor = root.walk();
        self.walk_calls(&mut cursor, root);
    }

    fn walk_calls(&mut self, cursor: &mut TreeCursor<'_>, root: TsNode<'_>) {
        let node = cursor.node();
        match node.kind() {
            "function_call_expression" => {
                if let Some(function) = node.child_by_field_name("function")
                    && let Some((name, name_node)) = callee_name(function, self.source)
                {
                    self.emit_call(node, name_node, name, "call", root);
                }
            }
            "member_call_expression" => {
                if let Some(name_node) = node.child_by_field_name("name")
                    && let Some(name) = simple_identifier(name_node, self.source)
                {
                    self.emit_call(node, name_node, name, "method_call", root);
                }
            }
            "nullsafe_member_call_expression" => {
                if let Some(name_node) = node.child_by_field_name("name")
                    && let Some(name) = simple_identifier(name_node, self.source)
                {
                    self.emit_call(node, name_node, name, "nullsafe_method_call", root);
                }
            }
            "scoped_call_expression" => {
                if let Some(name_node) = node.child_by_field_name("name")
                    && let Some(name) = simple_identifier(name_node, self.source)
                {
                    self.emit_call(node, name_node, name, "static_call", root);
                }
            }
            _ => {}
        }

        if cursor.goto_first_child() {
            loop {
                self.walk_calls(cursor, root);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            cursor.goto_parent();
        }
    }

    fn emit_call(
        &mut self,
        call: TsNode<'_>,
        _name_node: TsNode<'_>,
        name: String,
        kind: &'static str,
        root: TsNode<'_>,
    ) {
        let container = enclosing_definition_local_id(
            root,
            call.start_byte(),
            call.end_byte(),
            &self.container_index,
        );
        let id = self.refs.len() as LocalRefId;
        self.refs.push(Ref {
            id,
            kind: kind.to_string(),
            qname: Some(normalize_qualified_name(&name)),
            name,
            alias: None,
            span: span_from_node(call),
            container,
        });
    }

    fn collect_syntax_diagnostics(&mut self, root: TsNode<'_>) {
        if !root.has_error() && !root.is_missing() {
            return;
        }
        let mut cursor = root.walk();
        self.walk_syntax_errors(&mut cursor);
    }

    fn walk_syntax_errors(&mut self, cursor: &mut TreeCursor<'_>) {
        let node = cursor.node();
        if node.is_missing() {
            self.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message: format!("missing `{}` token", node.kind()),
                span: Some(span_from_node(node)),
            });
        } else if node.is_error() {
            self.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message: "syntax error".to_string(),
                span: Some(span_from_node(node)),
            });
        }

        if cursor.goto_first_child() {
            loop {
                self.walk_syntax_errors(cursor);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            cursor.goto_parent();
        }
    }

    fn slice_text(&self, node: TsNode<'_>) -> &str {
        let range = node.byte_range();
        std::str::from_utf8(&self.source[range]).unwrap_or_default()
    }
}

fn callee_name<'tree>(function: TsNode<'tree>, source: &[u8]) -> Option<(String, TsNode<'tree>)> {
    match function.kind() {
        "name" => {
            let text = std::str::from_utf8(&source[function.byte_range()]).ok()?;
            Some((text.to_string(), function))
        }
        "qualified_name" => {
            let text = std::str::from_utf8(&source[function.byte_range()]).ok()?;
            Some((text.to_string(), function))
        }
        "variable_name" => {
            let inner = function.child_by_field_name("name")?;
            let text = std::str::from_utf8(&source[inner.byte_range()]).ok()?;
            Some((format!("${text}"), inner))
        }
        _ => None,
    }
}

fn simple_identifier(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "name" => std::str::from_utf8(&source[node.byte_range()])
            .ok()
            .map(str::to_string),
        "variable_name" => {
            let inner = node.child_by_field_name("name")?;
            let text = std::str::from_utf8(&source[inner.byte_range()]).ok()?;
            Some(format!("${text}"))
        }
        _ => None,
    }
}

fn enclosing_namespace(root: TsNode<'_>, byte: usize, source: &[u8]) -> Option<String> {
    // PHP namespaces come in two forms:
    //   1. `namespace Foo { ... }` — block scope; the namespace_definition has
    //      a body that lexically contains its contents.
    //   2. `namespace Foo;` — file scope; the namespace_definition has no body
    //      and applies from the semicolon until either the next file-scope
    //      namespace declaration or EOF.
    //
    // Tree-sitter only nests block-scoped namespaces. File-scope ones are root
    // siblings of the rest of the file, so we resolve them by looking at the
    // most recent root-level `namespace_definition` whose end byte precedes
    // the query position. Block-scoped namespaces take precedence because
    // their byte ranges contain the query position directly.

    let mut block_scope: Option<(usize, String)> = None;
    let mut cursor = root.walk();
    walk_block_namespaces(&mut cursor, byte, source, &mut block_scope);
    if let Some((_, name)) = block_scope {
        return Some(name);
    }

    let mut file_scope: Option<(usize, String)> = None;
    let mut walker = root.walk();
    for child in root.named_children(&mut walker) {
        if child.kind() != "namespace_definition" {
            continue;
        }
        if child.child_by_field_name("body").is_some() {
            continue;
        }
        let end = child.end_byte();
        if end > byte {
            continue;
        }
        if file_scope
            .as_ref()
            .is_none_or(|(prev_end, _)| end >= *prev_end)
            && let Some(name_node) = child.child_by_field_name("name")
            && let Ok(text) = std::str::from_utf8(&source[name_node.byte_range()])
        {
            file_scope = Some((end, text.to_string()));
        }
    }
    file_scope.map(|(_, name)| name)
}

fn walk_block_namespaces(
    cursor: &mut TreeCursor<'_>,
    byte: usize,
    source: &[u8],
    best: &mut Option<(usize, String)>,
) {
    let node = cursor.node();
    if node.kind() == "namespace_definition"
        && let Some(body) = node.child_by_field_name("body")
    {
        let body_range = body.byte_range();
        if body_range.contains(&byte)
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(text) = std::str::from_utf8(&source[name_node.byte_range()])
        {
            let depth = body_range.end - body_range.start;
            if best.as_ref().is_none_or(|(d, _)| depth < *d) {
                *best = Some((depth, text.to_string()));
            }
        }
    }

    if cursor.goto_first_child() {
        loop {
            walk_block_namespaces(cursor, byte, source, best);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn enclosing_definition_local_id(
    root: TsNode<'_>,
    start: usize,
    end: usize,
    container_index: &std::collections::HashMap<usize, LocalNodeId>,
) -> Option<LocalNodeId> {
    let mut best: Option<(usize, LocalNodeId)> = None;
    let mut cursor = root.walk();
    walk_definition_containers(
        &mut cursor,
        start,
        end,
        container_index,
        &mut best,
        root.id(),
    );
    best.map(|(_, id)| id)
}

fn walk_definition_containers(
    cursor: &mut TreeCursor<'_>,
    start: usize,
    end: usize,
    container_index: &std::collections::HashMap<usize, LocalNodeId>,
    best: &mut Option<(usize, LocalNodeId)>,
    root_id: usize,
) {
    let node = cursor.node();
    if let Some(local_id) = container_index.get(&node.id())
        && node.id() != root_id
    {
        let range = node.byte_range();
        if range.start <= start && range.end >= end {
            let span = range.end - range.start;
            if best.as_ref().is_none_or(|(s, _)| span < *s) {
                *best = Some((span, *local_id));
            }
        }
    }

    if cursor.goto_first_child() {
        loop {
            walk_definition_containers(cursor, start, end, container_index, best, root_id);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn build_qname(
    namespace: Option<&str>,
    parent_qname: Option<&str>,
    name: &str,
    kind: &str,
) -> String {
    if let Some(parent) = parent_qname {
        return match kind {
            "method" | "property" | "constant" | "enum_case" => {
                format!("{parent}::{name}")
            }
            _ => format!("{parent}\\{name}"),
        };
    }

    if let Some(ns) = namespace {
        let ns = ns.trim_start_matches('\\');
        if ns.is_empty() {
            name.to_string()
        } else {
            format!("{ns}\\{name}")
        }
    } else {
        name.to_string()
    }
}

fn normalize_qualified_name(name: &str) -> String {
    name.trim_start_matches('\\').to_string()
}

fn last_segment(qname: &str) -> &str {
    qname.rsplit('\\').next().unwrap_or(qname)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn extract(source: &str) -> ExtractedFile {
        let plugin = PhpPlugin::new();
        plugin.extract(source.as_bytes(), &PathBuf::from("test.php"))
    }

    fn node_qnames(file: &ExtractedFile, kind: &str) -> Vec<String> {
        file.nodes
            .iter()
            .filter(|n| n.kind == kind)
            .map(|n| n.qname.clone())
            .collect()
    }

    fn call_names(file: &ExtractedFile) -> Vec<String> {
        file.refs
            .iter()
            .filter(|r| {
                matches!(
                    r.kind.as_str(),
                    "call" | "method_call" | "static_call" | "nullsafe_method_call"
                )
            })
            .map(|r| r.name.clone())
            .collect()
    }

    #[test]
    fn extracts_namespace_class_inheritance_and_members() {
        let source = include_str!("../../tests/fixtures/php/class_with_members.php");
        let extracted = extract(source);

        let namespaces = node_qnames(&extracted, "namespace");
        assert_eq!(namespaces, vec!["App\\Services".to_string()]);

        let classes = node_qnames(&extracted, "class");
        assert_eq!(classes, vec!["App\\Services\\OrderService".to_string()]);

        let methods = node_qnames(&extracted, "method");
        assert!(
            methods.contains(&"App\\Services\\OrderService::__construct".to_string()),
            "expected constructor, got {methods:?}"
        );
        assert!(
            methods.contains(&"App\\Services\\OrderService::place".to_string()),
            "expected place(), got {methods:?}"
        );

        let properties = node_qnames(&extracted, "property");
        assert!(
            properties.contains(&"App\\Services\\OrderService::$repository".to_string()),
            "expected $repository property, got {properties:?}"
        );

        let extends: Vec<_> = extracted
            .refs
            .iter()
            .filter(|r| r.kind == "extends")
            .filter_map(|r| r.qname.clone())
            .collect();
        assert_eq!(extends, vec!["BaseService".to_string()]);

        let implements: Vec<_> = extracted
            .refs
            .iter()
            .filter(|r| r.kind == "implements")
            .filter_map(|r| r.qname.clone())
            .collect();
        assert_eq!(implements, vec!["OrderContract".to_string()]);

        let trait_uses: Vec<_> = extracted
            .refs
            .iter()
            .filter(|r| r.kind == "trait_use")
            .filter_map(|r| r.qname.clone())
            .collect();
        assert_eq!(trait_uses, vec!["Loggable".to_string()]);

        let imports: Vec<_> = extracted
            .refs
            .iter()
            .filter(|r| r.kind == "import")
            .filter_map(|r| r.qname.clone())
            .collect();
        assert!(
            imports.contains(&"App\\Contracts\\OrderContract".to_string()),
            "expected OrderContract import, got {imports:?}"
        );
        assert!(
            imports.contains(&"App\\Support\\Loggable".to_string()),
            "expected Loggable import, got {imports:?}"
        );
        assert!(
            imports.contains(&"App\\Services\\BaseService".to_string()),
            "expected BaseService import, got {imports:?}"
        );
    }

    #[test]
    fn extracts_function_calls() {
        let source = include_str!("../../tests/fixtures/php/function_with_calls.php");
        let extracted = extract(source);

        let functions = node_qnames(&extracted, "function");
        assert!(
            functions.contains(&"App\\Util\\dispatch".to_string()),
            "expected dispatch() function, got {functions:?}"
        );

        let calls = call_names(&extracted);
        assert!(
            calls.contains(&"sprintf".to_string()),
            "missing sprintf call, got {calls:?}"
        );
        assert!(
            calls.contains(&"strtolower".to_string()),
            "missing strtolower call, got {calls:?}"
        );
        assert!(
            calls.contains(&"handle".to_string()),
            "missing handle method call, got {calls:?}"
        );
        assert!(
            calls.contains(&"resolve".to_string()),
            "missing resolve static call, got {calls:?}"
        );
        assert!(
            calls.contains(&"name".to_string()),
            "missing nullsafe name() call, got {calls:?}"
        );

        let dispatch_id = extracted
            .nodes
            .iter()
            .find(|n| n.qname == "App\\Util\\dispatch")
            .map(|n| n.id)
            .expect("dispatch function node");

        let sprintf_call = extracted
            .refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "sprintf")
            .expect("sprintf call ref");
        assert_eq!(sprintf_call.container, Some(dispatch_id));
    }

    #[test]
    fn malformed_input_still_yields_partial_nodes_and_diagnostics() {
        let source = include_str!("../../tests/fixtures/php/syntax_error.php");
        let extracted = extract(source);

        assert!(
            !extracted.diagnostics.is_empty(),
            "expected at least one diagnostic for malformed PHP"
        );

        let classes = node_qnames(&extracted, "class");
        assert!(
            classes.iter().any(|q| q.ends_with("Broken")),
            "expected partial class extraction for `Broken`, got {classes:?}"
        );
    }

    #[test]
    fn enums_are_classified_as_enum_not_class() {
        let source = include_str!("../../tests/fixtures/php/enum_status.php");
        let extracted = extract(source);

        let enums = node_qnames(&extracted, "enum");
        assert_eq!(enums, vec!["App\\Models\\Status".to_string()]);

        let classes = node_qnames(&extracted, "class");
        assert!(
            classes.is_empty(),
            "enum must not be reported as a class, got {classes:?}"
        );

        let cases = node_qnames(&extracted, "enum_case");
        assert!(
            cases.contains(&"App\\Models\\Status::Active".to_string()),
            "expected Active enum case, got {cases:?}"
        );
        assert!(
            cases.contains(&"App\\Models\\Status::Inactive".to_string()),
            "expected Inactive enum case, got {cases:?}"
        );
    }

    #[test]
    fn definition_query_compiles() {
        let _queries = compiled_queries();
    }
}
