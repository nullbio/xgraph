use std::collections::HashMap;
use std::path::Path;

use tree_sitter::{Language, Node as TsNode, Parser, Tree};

use crate::extract::{Diagnostic, ExtractedFile, LocalNodeId, LocalRefId, Node, Ref, Severity};
use crate::language::{LanguageId, LanguagePlugin, LanguageQueries};

use super::javascript::{ContainerRange, enclosing_def, slice_text, span_from_node};

static QUERIES: LanguageQueries = LanguageQueries {
    definitions: tree_sitter_rust::TAGS_QUERY,
    imports: "",
    exports: "",
    types: "",
    routes: "",
};

pub struct RustPlugin;

impl LanguagePlugin for RustPlugin {
    fn id(&self) -> LanguageId {
        LanguageId::Rust
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
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
                message: "rust parser failed to produce a tree".into(),
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
    tree_sitter_rust::LANGUAGE.into()
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
    RustPlugin.extract(source, Path::new(""))
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
        self.collect_named_types(root, None);
        self.collect_items(root, None);
    }

    fn collect_named_types(&mut self, node: TsNode<'_>, parent: Option<LocalNodeId>) {
        match node.kind() {
            "struct_item" => {
                self.collect_named_type(node, "struct", parent);
                return;
            }
            "enum_item" => {
                self.collect_named_type(node, "enum", parent);
                return;
            }
            "trait_item" => {
                self.collect_named_type(node, "trait", parent);
                return;
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_named_types(child, parent);
        }
    }

    fn collect_items(&mut self, node: TsNode<'_>, parent: Option<LocalNodeId>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "use_declaration" => self.collect_use_declaration(child),
                "function_item" => {
                    if !is_inside_impl_or_trait(child) {
                        self.collect_function(child, parent, "function", None);
                    }
                }
                "struct_item" | "enum_item" | "trait_item" => {
                    let type_parent = self
                        .name_for_node(child)
                        .and_then(|name| self.type_ids.get(&name).copied());
                    if child.kind() == "trait_item" {
                        self.collect_trait_body(child, type_parent);
                    }
                }
                "type_item" => {
                    self.collect_type_alias(child, parent);
                }
                "const_item" => {
                    self.collect_named_item(child, "constant", parent);
                }
                "static_item" => {
                    self.collect_named_item(child, "variable", parent);
                }
                "mod_item" => {
                    self.collect_mod_item(child, parent);
                }
                "impl_item" => self.collect_impl_item(child),
                _ => self.collect_items(child, parent),
            }
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

    fn name_for_node(&self, node: TsNode<'_>) -> Option<String> {
        node.child_by_field_name("name")
            .map(|n| slice_text(self.source, n))
    }

    fn collect_named_type(
        &mut self,
        node: TsNode<'_>,
        kind: &'static str,
        parent: Option<LocalNodeId>,
    ) -> Option<LocalNodeId> {
        let name = self.name_for_node(node)?;
        if self.type_ids.contains_key(&name) {
            return self.type_ids.get(&name).copied();
        }
        let id = self.push_node(kind, name.clone(), name.clone(), node, parent);
        self.type_ids.insert(name, id);
        Some(id)
    }

    fn collect_named_item(
        &mut self,
        node: TsNode<'_>,
        kind: &'static str,
        parent: Option<LocalNodeId>,
    ) -> Option<LocalNodeId> {
        let name = self.name_for_node(node)?;
        Some(self.push_node(kind, name.clone(), name, node, parent))
    }

    fn collect_type_alias(
        &mut self,
        node: TsNode<'_>,
        parent: Option<LocalNodeId>,
    ) -> Option<LocalNodeId> {
        let name = self.name_for_node(node)?;
        Some(self.push_node("type_alias", name.clone(), name, node, parent))
    }

    fn collect_mod_item(
        &mut self,
        node: TsNode<'_>,
        parent: Option<LocalNodeId>,
    ) -> Option<LocalNodeId> {
        let name = self.name_for_node(node)?;
        let id = self.push_node("module", name.clone(), name, node, parent);
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_named_types(body, Some(id));
            self.collect_items(body, Some(id));
        }
        Some(id)
    }

    fn collect_function(
        &mut self,
        node: TsNode<'_>,
        parent: Option<LocalNodeId>,
        kind: &'static str,
        owner_name: Option<&str>,
    ) -> Option<LocalNodeId> {
        let name = self.name_for_node(node)?;
        let qname = owner_name
            .map(|owner| format!("{owner}.{name}"))
            .unwrap_or_else(|| name.clone());
        let id = self.push_node(kind, name.clone(), qname, node, parent);
        self.collect_route_attributes(node, id, &name);
        Some(id)
    }

    fn collect_trait_body(&mut self, node: TsNode<'_>, trait_id: Option<LocalNodeId>) {
        let trait_name = self.name_for_node(node);
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "function_signature_item" | "function_item" => {
                    self.collect_function(child, trait_id, "method", trait_name.as_deref());
                }
                _ => {}
            }
        }
    }

    fn collect_impl_item(&mut self, node: TsNode<'_>) {
        let receiver = node
            .child_by_field_name("type")
            .map(|n| rust_type_name(&slice_text(self.source, n)))
            .filter(|s| !s.is_empty());
        let parent = receiver
            .as_ref()
            .and_then(|ty| self.type_ids.get(ty).copied());

        if let (Some(trait_node), Some(parent_id)) = (node.child_by_field_name("trait"), parent) {
            let trait_name = normalize_rust_path(&slice_text(self.source, trait_node), "");
            if !trait_name.is_empty() {
                self.push_ref(
                    "implements",
                    rust_tail(&trait_name).to_owned(),
                    Some(trait_name),
                    None,
                    trait_node,
                    Some(parent_id),
                );
            }
        }

        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "function_item" | "function_signature_item" => {
                    self.collect_function(child, parent, "method", receiver.as_deref());
                }
                "const_item" => {
                    self.collect_named_item(child, "constant", parent);
                }
                _ => {}
            }
        }
    }

    fn collect_route_attributes(
        &mut self,
        function: TsNode<'_>,
        function_id: LocalNodeId,
        name: &str,
    ) {
        let mut previous = function.prev_named_sibling();
        while let Some(attr_node) = previous {
            if attr_node.kind() != "attribute_item" {
                break;
            }
            self.collect_route_attribute_node(attr_node, function, function_id, name);
            previous = attr_node.prev_named_sibling();
        }

        let mut cursor = function.walk();
        for child in function.named_children(&mut cursor) {
            if child.kind() != "attribute_item" {
                continue;
            }
            self.collect_route_attribute_node(child, function, function_id, name);
        }
    }

    fn collect_route_attribute_node(
        &mut self,
        attr_node: TsNode<'_>,
        function: TsNode<'_>,
        function_id: LocalNodeId,
        name: &str,
    ) {
        let attr = slice_text(self.source, attr_node);
        let Some((method, path)) = parse_rust_route_attribute(&attr) else {
            return;
        };
        let route_name = format!("{method} {path}");
        let route_id = self.push_node(
            "route",
            route_name.clone(),
            format!("route:{route_name}"),
            attr_node,
            None,
        );
        self.push_ref(
            "route_handler",
            name.to_owned(),
            None,
            None,
            attr_node,
            Some(route_id),
        );
        self.push_ref(
            "route_handler",
            name.to_owned(),
            None,
            None,
            function,
            Some(function_id),
        );
    }

    fn collect_use_declaration(&mut self, node: TsNode<'_>) {
        let raw = slice_text(self.source, node);
        for binding in parse_use_bindings(&raw) {
            self.push_ref(
                "import_rust",
                binding.name,
                Some(binding.qname),
                binding.alias,
                node,
                None,
            );
        }
    }

    fn walk_for_refs(&mut self, root: TsNode<'_>) {
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            match node.kind() {
                "call_expression" => self.collect_call(node),
                "macro_invocation" => self.collect_macro_invocation(node),
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
        let raw = call_name(function, self.source);
        if raw.is_empty() {
            return;
        }
        let container = enclosing_def(&self.container_ranges, node.start_byte(), node.end_byte());
        let normalized = normalize_rust_path(&raw, "");
        let qname = if normalized.contains("::") {
            Some(normalized.clone())
        } else {
            None
        };
        self.push_ref(
            "rust_call",
            normalized.clone(),
            qname,
            None,
            node,
            container,
        );
        self.collect_axum_route(node, &raw);
    }

    fn collect_axum_route(&mut self, node: TsNode<'_>, raw_name: &str) {
        if !raw_name.ends_with(".route") && !raw_name.ends_with("::route") && raw_name != "route" {
            return;
        }
        let text = slice_text(self.source, node);
        let Some((path, handlers)) = parse_axum_route_call(&text) else {
            return;
        };
        for (method, handler) in handlers {
            let route_name = format!("{method} {path}");
            let route_id = self.push_node(
                "route",
                route_name.clone(),
                format!("route:{route_name}"),
                node,
                None,
            );
            self.push_ref("route_handler", handler, None, None, node, Some(route_id));
        }
    }

    fn collect_macro_invocation(&mut self, node: TsNode<'_>) {
        let text = slice_text(self.source, node);
        let Some(body) = text.strip_prefix("routes!") else {
            return;
        };
        let Some(start) = body.find('[') else {
            return;
        };
        let Some(end) = body.rfind(']') else {
            return;
        };
        let container = enclosing_def(&self.container_ranges, node.start_byte(), node.end_byte());
        for path in body[start + 1..end].split(',') {
            let qname = normalize_rust_path(path.trim(), "");
            if qname.is_empty() {
                continue;
            }
            self.push_ref(
                "route_handler",
                rust_tail(&qname).to_owned(),
                Some(qname),
                None,
                node,
                container,
            );
        }
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

fn is_inside_impl_or_trait(node: TsNode<'_>) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        match p.kind() {
            "impl_item" | "trait_item" => return true,
            "source_file" => return false,
            _ => parent = p.parent(),
        }
    }
    false
}

fn rust_type_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_ref = trimmed.trim_start_matches('&').trim_start_matches("mut ");
    let base = without_ref
        .split(['<', ' ', '\n', '\t'])
        .next()
        .unwrap_or(without_ref);
    rust_tail(base).to_owned()
}

fn call_name(node: TsNode<'_>, source: &[u8]) -> String {
    match node.kind() {
        "identifier" | "type_identifier" => slice_text(source, node),
        "scoped_identifier" | "scoped_type_identifier" => {
            normalize_rust_path(&slice_text(source, node), "")
        }
        "field_expression" => {
            let Some(field) = node.child_by_field_name("field") else {
                return String::new();
            };
            let object = node
                .child_by_field_name("value")
                .or_else(|| node.child_by_field_name("argument"));
            let field = slice_text(source, field);
            if let Some(object) = object {
                let object_name = call_name(object, source);
                if object_name.is_empty() {
                    field
                } else {
                    format!("{object_name}.{field}")
                }
            } else {
                field
            }
        }
        "generic_function" => node
            .child_by_field_name("function")
            .map(|n| call_name(n, source))
            .unwrap_or_default(),
        _ => slice_text(source, node),
    }
}

fn normalize_rust_path(raw: &str, current_module: &str) -> String {
    let path = raw.trim().trim_end_matches(';').replace([' ', '\n'], "");
    if let Some(rest) = path.strip_prefix("crate::") {
        return rest.to_owned();
    }
    if let Some(rest) = path.strip_prefix("self::") {
        return if current_module.is_empty() {
            rest.to_owned()
        } else {
            format!("{current_module}::{rest}")
        };
    }
    if let Some(rest) = path.strip_prefix("super::") {
        let parent = current_module
            .rsplit_once("::")
            .map(|(p, _)| p)
            .unwrap_or("");
        return if parent.is_empty() {
            rest.to_owned()
        } else {
            format!("{parent}::{rest}")
        };
    }
    path
}

fn rust_tail(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

#[derive(Debug, PartialEq, Eq)]
struct UseBinding {
    name: String,
    qname: String,
    alias: Option<String>,
}

fn parse_use_bindings(raw: &str) -> Vec<UseBinding> {
    let Some(rest) = raw.trim().strip_prefix("use ") else {
        return Vec::new();
    };
    let body = rest.trim().trim_end_matches(';').trim();
    let mut out = Vec::new();
    if let (Some(open), Some(close)) = (body.find('{'), body.rfind('}')) {
        let prefix = body[..open].trim().trim_end_matches("::");
        for part in body[open + 1..close].split(',') {
            let item = part.trim();
            if item.is_empty() {
                continue;
            }
            let (path, alias) = split_alias(item);
            let qname = if path == "self" {
                normalize_rust_path(prefix, "")
            } else if prefix.is_empty() {
                normalize_rust_path(path, "")
            } else {
                normalize_rust_path(&format!("{prefix}::{path}"), "")
            };
            out.push(UseBinding {
                name: rust_tail(&qname).to_owned(),
                qname,
                alias,
            });
        }
        return out;
    }

    let (path, alias) = split_alias(body);
    let qname = normalize_rust_path(path, "");
    out.push(UseBinding {
        name: rust_tail(&qname).to_owned(),
        qname,
        alias,
    });
    out
}

fn split_alias(path: &str) -> (&str, Option<String>) {
    if let Some((left, right)) = path.split_once(" as ") {
        (left.trim(), Some(right.trim().to_owned()))
    } else {
        (path.trim(), None)
    }
}

fn parse_rust_route_attribute(attr: &str) -> Option<(String, String)> {
    let inner = attr.trim().strip_prefix("#[")?.strip_suffix(']')?;
    let open = inner.find('(')?;
    let method = inner[..open].trim();
    if !matches!(
        method,
        "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
    ) {
        return None;
    }
    let rest = &inner[open + 1..];
    let first_quote = rest.find(['"', '\''])?;
    let quote = rest.as_bytes()[first_quote] as char;
    let tail = &rest[first_quote + 1..];
    let end = tail.find(quote)?;
    Some((method.to_ascii_uppercase(), tail[..end].to_owned()))
}

fn parse_axum_route_call(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let first_quote = text.find('"')?;
    let tail = &text[first_quote + 1..];
    let end_quote = tail.find('"')?;
    let path = tail[..end_quote].to_owned();
    let after_path = &tail[end_quote + 1..];
    let mut handlers = Vec::new();
    for method in [
        "get", "post", "put", "patch", "delete", "head", "options", "trace",
    ] {
        let mut rest = after_path;
        let needle = format!("{method}(");
        while let Some(idx) = rest.find(&needle) {
            let start = idx + needle.len();
            let handler_src = &rest[start..];
            let handler: String = handler_src
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            if !handler.is_empty() {
                handlers.push((method.to_ascii_uppercase(), rust_tail(&handler).to_owned()));
            }
            rest = &handler_src[handler.len()..];
        }
    }
    if handlers.is_empty() {
        None
    } else {
        Some((path, handlers))
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
    fn captures_rust_items_traits_and_impl_methods() {
        let src = br#"
pub struct Server;
pub enum Mode { Fast }
pub trait Runner { fn run(&self); }
type Count = usize;

impl Runner for Server {
    fn run(&self) {}
}
"#;
        let out = extract(src);
        assert_eq!(find_node(&out, "Server").unwrap().kind, "struct");
        assert_eq!(find_node(&out, "Mode").unwrap().kind, "enum");
        assert_eq!(find_node(&out, "Runner").unwrap().kind, "trait");
        assert_eq!(find_node(&out, "Count").unwrap().kind, "type_alias");

        let run = find_node(&out, "run").unwrap();
        assert_eq!(run.kind, "method");
        assert_eq!(run.qname, "Runner.run");

        let implements = refs_of(&out, "implements");
        assert!(implements.iter().any(|r| r.name == "Runner"));
    }

    #[test]
    fn captures_use_bindings_and_scoped_calls() {
        let src = br#"
use crate::server::{start, Server as S};

fn main() {
    start();
    crate::server::stop();
}
"#;
        let out = extract(src);
        let imports = refs_of(&out, "import_rust");
        assert!(
            imports
                .iter()
                .any(|r| r.qname.as_deref() == Some("server::start"))
        );
        assert!(imports.iter().any(|r| {
            r.qname.as_deref() == Some("server::Server") && r.alias.as_deref() == Some("S")
        }));

        let calls = refs_of(&out, "rust_call");
        assert!(calls.iter().any(|r| r.name == "start"));
        assert!(
            calls
                .iter()
                .any(|r| r.qname.as_deref() == Some("server::stop"))
        );
    }

    #[test]
    fn captures_rust_route_attributes_and_axum_routes() {
        let src = br#"
#[get("/users")]
async fn list_users() {}

fn router() {
    Router::new().route("/health", get(health).post(api::create));
}
"#;
        let out = extract(src);
        assert!(find_node(&out, "GET /users").is_some());
        assert!(find_node(&out, "GET /health").is_some());
        assert!(find_node(&out, "POST /health").is_some());
        let handlers = refs_of(&out, "route_handler");
        assert!(handlers.iter().any(|r| r.name == "list_users"));
        assert!(handlers.iter().any(|r| r.name == "health"));
        assert!(handlers.iter().any(|r| r.name == "create"));
    }
}
