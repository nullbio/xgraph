use std::collections::HashMap;
use std::path::Path;

use tree_sitter::{Language, Node as TsNode, Parser, Tree};

use crate::extract::{Diagnostic, ExtractedFile, LocalNodeId, LocalRefId, Node, Ref, Severity};
use crate::language::{LanguageId, LanguagePlugin, LanguageQueries};

use super::javascript::{ContainerRange, enclosing_def, slice_text, span_from_node};

static QUERIES: LanguageQueries = LanguageQueries {
    definitions: tree_sitter_go::TAGS_QUERY,
    imports: "",
    exports: "",
    types: "",
    routes: "",
};

pub struct GoPlugin;

impl LanguagePlugin for GoPlugin {
    fn id(&self) -> LanguageId {
        LanguageId::Go
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn queries(&self) -> &'static LanguageQueries {
        &QUERIES
    }

    fn tree_sitter_language(&self) -> Language {
        language()
    }

    fn extract(&self, source: &[u8], path: &Path) -> ExtractedFile {
        let mut extractor = Extractor::new(source, path);
        let Some(tree) = parse(source) else {
            extractor.out.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message: "go parser failed to produce a tree".into(),
                span: None,
            });
            return extractor.out;
        };
        let root = tree.root_node();
        extractor.collect_definitions(root);
        extractor.walk_for_refs(root);
        extractor.collect_diagnostics(root);
        extractor.out
    }
}

fn language() -> Language {
    tree_sitter_go::LANGUAGE.into()
}

thread_local! {
    static PARSER: std::cell::RefCell<Option<Parser>> = const { std::cell::RefCell::new(None) };
}

pub fn parse(source: &[u8]) -> Option<Tree> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut p = Parser::new();
            p.set_language(&language()).ok()?;
            *slot = Some(p);
        }
        slot.as_mut().unwrap().parse(source, None)
    })
}

pub fn extract(source: &[u8]) -> ExtractedFile {
    GoPlugin.extract(source, Path::new(""))
}

struct Extractor<'a> {
    out: ExtractedFile,
    source: &'a [u8],
    next_node_id: LocalNodeId,
    next_ref_id: LocalRefId,
    container_ranges: Vec<ContainerRange>,
    type_ids: HashMap<String, LocalNodeId>,
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
            type_ids: HashMap::new(),
        }
    }

    fn collect_definitions(&mut self, root: TsNode<'_>) {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            self.collect_top_level(child);
        }
    }

    fn collect_top_level(&mut self, node: TsNode<'_>) {
        match node.kind() {
            "import_declaration" => self.collect_import_declaration(node),
            "type_declaration" => self.collect_type_declaration(node),
            "function_declaration" => {
                self.collect_function(node);
            }
            "method_declaration" => {
                self.collect_method(node);
            }
            "const_declaration" => self.collect_var_const_declaration(node, "constant"),
            "var_declaration" => self.collect_var_const_declaration(node, "variable"),
            _ => {}
        }
    }

    fn push_node(
        &mut self,
        kind: &'static str,
        name: String,
        qname: String,
        node: TsNode<'_>,
        parent: Option<LocalNodeId>,
    ) -> LocalNodeId {
        let id = self.next_node_id;
        self.next_node_id += 1;
        self.container_ranges
            .push((node.start_byte(), node.end_byte(), id));
        self.out.nodes.push(Node {
            id,
            kind: kind.to_owned(),
            name,
            qname,
            span: span_from_node(node),
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
        node: TsNode<'_>,
        container: Option<LocalNodeId>,
    ) {
        let id = self.next_ref_id;
        self.next_ref_id += 1;
        self.out.refs.push(Ref {
            id,
            kind: kind.to_owned(),
            name,
            qname,
            alias,
            span: span_from_node(node),
            container,
        });
    }

    fn collect_import_declaration(&mut self, node: TsNode<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "import_spec" => self.collect_import_spec(child),
                "import_spec_list" => {
                    let mut list_cursor = child.walk();
                    for spec in child.named_children(&mut list_cursor) {
                        if spec.kind() == "import_spec" {
                            self.collect_import_spec(spec);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_import_spec(&mut self, spec: TsNode<'_>) {
        let Some(path_node) = spec.child_by_field_name("path") else {
            return;
        };
        let raw = slice_text(self.source, path_node);
        let import_path = strip_go_string(&raw).to_owned();
        if import_path.is_empty() {
            return;
        }
        let alias = spec
            .child_by_field_name("name")
            .map(|n| slice_text(self.source, n))
            .filter(|s| s != "_" && s != ".");
        let package_name = alias
            .clone()
            .unwrap_or_else(|| package_name_from_path(&import_path));
        self.push_ref(
            "import_go",
            import_path,
            None,
            Some(package_name),
            spec,
            None,
        );
    }

    fn collect_type_declaration(&mut self, node: TsNode<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "type_spec" || child.kind() == "type_alias" {
                self.collect_type_spec(child);
            }
        }
    }

    fn collect_type_spec(&mut self, node: TsNode<'_>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = slice_text(self.source, name_node);
        let kind = match node.child_by_field_name("type").map(|n| n.kind()) {
            Some("struct_type") => "struct",
            Some("interface_type") => "interface",
            _ => "type_alias",
        };
        let id = self.push_node(kind, name.clone(), name.clone(), node, None);
        if matches!(kind, "struct" | "interface") {
            self.type_ids.insert(name, id);
        }
    }

    fn collect_function(&mut self, node: TsNode<'_>) -> Option<LocalNodeId> {
        let name = node
            .child_by_field_name("name")
            .map(|n| slice_text(self.source, n))?;
        Some(self.push_node("function", name.clone(), name, node, None))
    }

    fn collect_method(&mut self, node: TsNode<'_>) -> Option<LocalNodeId> {
        let name = node
            .child_by_field_name("name")
            .map(|n| slice_text(self.source, n))?;
        let receiver = node
            .child_by_field_name("receiver")
            .and_then(|n| receiver_type(&slice_text(self.source, n)));
        let qname = receiver
            .as_deref()
            .map(|ty| format!("{ty}.{name}"))
            .unwrap_or_else(|| name.clone());
        let parent = receiver
            .as_ref()
            .and_then(|ty| self.type_ids.get(ty).copied());
        Some(self.push_node("method", name, qname, node, parent))
    }

    fn collect_var_const_declaration(&mut self, node: TsNode<'_>, kind: &'static str) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "var_spec" | "const_spec" => self.collect_var_const_spec(child, kind),
                "var_spec_list" | "const_spec_list" => {
                    let mut list_cursor = child.walk();
                    for spec in child.named_children(&mut list_cursor) {
                        if spec.kind() == "var_spec" || spec.kind() == "const_spec" {
                            self.collect_var_const_spec(spec, kind);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_var_const_spec(&mut self, spec: TsNode<'_>, kind: &'static str) {
        let mut cursor = spec.walk();
        for child in spec.named_children(&mut cursor) {
            if child.kind() == "identifier" {
                let name = slice_text(self.source, child);
                self.push_node(kind, name.clone(), name, spec, None);
            }
        }
    }

    fn walk_for_refs(&mut self, root: TsNode<'_>) {
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            match node.kind() {
                "call_expression" => self.collect_call(node),
                "composite_literal" => self.collect_composite_literal(node),
                _ => {}
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

    fn collect_call(&mut self, node: TsNode<'_>) {
        let Some(function) = node.child_by_field_name("function") else {
            return;
        };
        let container = enclosing_def(&self.container_ranges, node.start_byte(), node.end_byte());
        if let Some((base, field)) = selector_parts(function, self.source) {
            let name = format!("{base}.{field}");
            self.push_ref(
                "go_selector_call",
                name,
                Some(format!("{base}#{field}")),
                None,
                node,
                container,
            );
            self.collect_route_call(node, &field);
        } else {
            let name = call_name(function, self.source);
            if !name.is_empty() {
                self.push_ref("call", name, None, None, node, container);
            }
        }
    }

    fn collect_route_call(&mut self, node: TsNode<'_>, method: &str) {
        if !is_go_route_method(method) {
            return;
        }
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let mut named = Vec::new();
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            named.push(child);
        }
        if named.len() < 2 {
            return;
        }
        let route_path = strip_go_string(&slice_text(self.source, named[0])).to_owned();
        if route_path.is_empty() || !route_path.starts_with('/') {
            return;
        }
        let Some(handler) = tail_identifier(named[1], self.source) else {
            return;
        };
        let route_name = format!("{} {}", normalized_go_route_method(method), route_path);
        let route_id = self.push_node(
            "route",
            route_name.clone(),
            format!("route:{route_name}"),
            node,
            None,
        );
        self.push_ref(
            "route_handler",
            handler,
            None,
            None,
            named[1],
            Some(route_id),
        );
    }

    fn collect_composite_literal(&mut self, node: TsNode<'_>) {
        let Some(ty) = node.child_by_field_name("type") else {
            return;
        };
        if !matches!(
            ty.kind(),
            "type_identifier" | "qualified_type" | "generic_type"
        ) {
            return;
        }
        let mut name = slice_text(self.source, ty);
        if let Some(idx) = name.find('[') {
            name.truncate(idx);
        }
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        let container = enclosing_def(&self.container_ranges, node.start_byte(), node.end_byte());
        let qname = name
            .split_once('.')
            .map(|(base, field)| format!("{base}#{field}"));
        self.push_ref("instantiates", name, qname, None, node, container);
    }

    fn collect_diagnostics(&mut self, root: TsNode<'_>) {
        if !root.has_error() {
            return;
        }
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            if node.is_error() || node.is_missing() {
                self.out.diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    message: if node.is_missing() {
                        format!("missing token: {}", node.kind())
                    } else {
                        "syntax error".to_owned()
                    },
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
}

fn strip_go_string(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'' || first == b'`') && first == last {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    trimmed
}

fn package_name_from_path(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).replace('-', "_")
}

fn receiver_type(receiver: &str) -> Option<String> {
    let inner = receiver
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    let candidate = inner.split_whitespace().last().unwrap_or(inner);
    let candidate = candidate.trim_start_matches('*').trim_start_matches('&');
    let name: String = candidate
        .chars()
        .skip_while(|c| !is_ident_start(*c))
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn selector_parts(node: TsNode<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "selector_expression" {
        return None;
    }
    let operand = node.child_by_field_name("operand")?;
    let field = node.child_by_field_name("field")?;
    let base = call_name(operand, source);
    let field = slice_text(source, field);
    if base.is_empty() || field.is_empty() {
        None
    } else {
        Some((base, field))
    }
}

fn call_name(node: TsNode<'_>, source: &[u8]) -> String {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "package_identifier" => {
            slice_text(source, node)
        }
        "selector_expression" => selector_parts(node, source)
            .map(|(base, field)| format!("{base}.{field}"))
            .unwrap_or_default(),
        "qualified_type" => slice_text(source, node),
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .next()
                .map(|n| call_name(n, source))
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn tail_identifier(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let text = call_name(node, source);
    if text.is_empty() {
        return None;
    }
    text.rsplit('.').next().map(str::to_owned)
}

fn is_go_route_method(method: &str) -> bool {
    matches!(
        method,
        "GET"
            | "POST"
            | "PUT"
            | "PATCH"
            | "DELETE"
            | "OPTIONS"
            | "HEAD"
            | "Get"
            | "Post"
            | "Put"
            | "Patch"
            | "Delete"
            | "Handle"
            | "HandleFunc"
    )
}

fn normalized_go_route_method(method: &str) -> &str {
    match method {
        "Handle" | "HandleFunc" => "ANY",
        "Get" => "GET",
        "Post" => "POST",
        "Put" => "PUT",
        "Patch" => "PATCH",
        "Delete" => "DELETE",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_node<'a>(out: &'a ExtractedFile, name: &str) -> Option<&'a Node> {
        out.nodes.iter().find(|n| n.name == name)
    }

    fn refs_of<'a>(out: &'a ExtractedFile, kind: &str) -> Vec<&'a Ref> {
        out.refs.iter().filter(|r| r.kind == kind).collect()
    }

    #[test]
    fn captures_go_functions_types_and_methods() {
        let src = br#"
package server

type Server struct {}
type Runner interface { Run() }
type Count = int

func NewServer() *Server { return &Server{} }
func (s *Server) Run() { NewServer() }
"#;
        let out = extract(src);

        assert_eq!(find_node(&out, "Server").unwrap().kind, "struct");
        assert_eq!(find_node(&out, "Runner").unwrap().kind, "interface");
        assert_eq!(find_node(&out, "Count").unwrap().kind, "type_alias");
        assert_eq!(find_node(&out, "NewServer").unwrap().kind, "function");

        let run = find_node(&out, "Run").unwrap();
        assert_eq!(run.kind, "method");
        assert_eq!(run.qname, "Server.Run");
        assert_eq!(run.parent, Some(find_node(&out, "Server").unwrap().id));
    }

    #[test]
    fn captures_go_import_aliases_and_selector_calls() {
        let src = br#"
package main

import srv "example.com/project/internal/server"

func main() {
    srv.Start()
}
"#;
        let out = extract(src);
        let import = refs_of(&out, "import_go")
            .into_iter()
            .find(|r| r.name == "example.com/project/internal/server")
            .unwrap();
        assert_eq!(import.alias.as_deref(), Some("srv"));

        let call = refs_of(&out, "go_selector_call")
            .into_iter()
            .find(|r| r.name == "srv.Start")
            .unwrap();
        assert_eq!(call.qname.as_deref(), Some("srv#Start"));
    }

    #[test]
    fn captures_common_go_route_handler_refs() {
        let src = br#"
package main

func register(r Router) {
    r.GET("/users", handlers.ListUsers)
}
"#;
        let out = extract(src);
        let route = find_node(&out, "GET /users").expect("route captured");
        assert_eq!(route.kind, "route");
        let handler = refs_of(&out, "route_handler")
            .into_iter()
            .find(|r| r.name == "ListUsers")
            .expect("handler ref captured");
        assert_eq!(handler.container, Some(route.id));
    }
}
