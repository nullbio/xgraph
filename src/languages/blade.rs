use std::path::Path;

use tree_sitter::{Node as TsNode, Parser, Tree};

use crate::extract::{Diagnostic, ExtractedFile, Node, Position, Ref, Severity, Span};
use crate::language::{LanguageId, LanguagePlugin, LanguageQueries};

mod ffi {
    #![allow(unsafe_code)]

    use tree_sitter::Language;
    use tree_sitter_language::LanguageFn;

    unsafe extern "C" {
        fn tree_sitter_blade() -> *const ();
    }

    const LANGUAGE_FN: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_blade) };

    pub(super) fn language() -> Language {
        LANGUAGE_FN.into()
    }
}

pub fn language() -> tree_sitter::Language {
    ffi::language()
}

static QUERIES: LanguageQueries = LanguageQueries {
    definitions: "",
    imports: "",
    exports: "",
    types: "",
    routes: "",
};

pub struct BladePlugin;

impl LanguagePlugin for BladePlugin {
    fn id(&self) -> LanguageId {
        LanguageId::Blade
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["blade.php"]
    }

    fn queries(&self) -> &'static LanguageQueries {
        &QUERIES
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        language()
    }

    fn extract(&self, source: &[u8], path: &Path) -> ExtractedFile {
        let mut extractor = Extractor::default();
        match parse(source) {
            Ok(tree) => {
                let root = tree.root_node();
                extractor.walk(root, source);
                extractor.collect_diagnostics(root, source);
            }
            Err(err) => {
                extractor.diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    message: err.0,
                    span: None,
                });
            }
        }
        ExtractedFile {
            path: path.to_path_buf(),
            nodes: extractor.nodes,
            refs: extractor.refs,
            diagnostics: extractor.diagnostics,
        }
    }
}

#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

thread_local! {
    static PARSER: std::cell::RefCell<Option<Parser>> = const { std::cell::RefCell::new(None) };
}

pub fn parse(source: &[u8]) -> Result<Tree, ParseError> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            parser
                .set_language(&language())
                .map_err(|e| ParseError(format!("set_language: {e}")))?;
            *slot = Some(parser);
        }
        slot.as_mut()
            .unwrap()
            .parse(source, None)
            .ok_or_else(|| ParseError("parser returned no tree".to_owned()))
    })
}

#[derive(Default)]
struct Extractor {
    nodes: Vec<Node>,
    refs: Vec<Ref>,
    diagnostics: Vec<Diagnostic>,
    next_node_id: u32,
    next_ref_id: u32,
}

impl Extractor {
    fn alloc_node_id(&mut self) -> u32 {
        let id = self.next_node_id;
        self.next_node_id += 1;
        id
    }

    fn alloc_ref_id(&mut self) -> u32 {
        let id = self.next_ref_id;
        self.next_ref_id += 1;
        id
    }

    fn push_node(&mut self, kind: &'static str, name: Option<String>, span: Span) {
        let id = self.alloc_node_id();
        self.nodes.push(Node {
            id,
            kind: kind.to_owned(),
            name: name.unwrap_or_default(),
            qname: String::new(),
            span,
            parent: None,
        });
    }

    fn push_ref(&mut self, kind: &'static str, name: String, span: Span) {
        let id = self.alloc_ref_id();
        self.refs.push(Ref {
            id,
            kind: kind.to_owned(),
            name,
            qname: None,
            alias: None,
            span,
            container: None,
        });
    }

    fn walk(&mut self, node: TsNode<'_>, source: &[u8]) {
        match node.kind() {
            "section" => self.handle_section(node, source),
            "stack" => self.handle_stack(node, source),
            "conditional" => self.handle_conditional(node, source),
            "php_statement" => self.handle_php_statement(node, source),
            "element" => self.handle_element(node, source),
            "directive" => self.handle_inline_directive(node, source),
            _ => {}
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                self.walk(cursor.node(), source);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn handle_section(&mut self, node: TsNode<'_>, source: &[u8]) {
        let directive_name = block_directive_name(node, source);
        let parameter_text = first_parameter_text(node, source);
        let section_name = parameter_text.as_deref().and_then(first_string_literal);

        self.push_node("blade_section", section_name.clone(), span_from_node(node));

        if directive_name.as_deref() == Some("@section")
            && let Some(name) = section_name
            && let Some(param_node) = first_parameter_node(node)
        {
            self.push_ref("blade_section_ref", name, span_from_node(param_node));
        }
    }

    fn handle_stack(&mut self, node: TsNode<'_>, source: &[u8]) {
        let directive_name = block_directive_name(node, source);
        let kind = match directive_name.as_deref() {
            Some("@prepend" | "@prependOnce") => "blade_prepend_block",
            _ => "blade_push_block",
        };
        let parameter_text = first_parameter_text(node, source);
        let stack_name = parameter_text.as_deref().and_then(first_string_literal);
        self.push_node(kind, stack_name.clone(), span_from_node(node));
        if let Some(name) = stack_name
            && let Some(param_node) = first_parameter_node(node)
        {
            self.push_ref("blade_stack_ref", name, span_from_node(param_node));
        }
    }

    fn handle_conditional(&mut self, node: TsNode<'_>, source: &[u8]) {
        let Some(name) = block_directive_name(node, source) else {
            return;
        };
        match name.as_str() {
            "@component" => self.emit_component(node, source),
            "@php" => self.push_node("blade_php_block", None, span_from_node(node)),
            _ => self.push_node("blade_directive_block", Some(name), span_from_node(node)),
        }
    }

    fn emit_component(&mut self, node: TsNode<'_>, source: &[u8]) {
        let parameter_text = first_parameter_text(node, source);
        let component_name = parameter_text.as_deref().and_then(first_string_literal);
        self.push_node(
            "blade_component_block",
            component_name.clone(),
            span_from_node(node),
        );
        if let Some(name) = component_name
            && let Some(param_node) = first_parameter_node(node)
        {
            self.push_ref("blade_component", name, span_from_node(param_node));
        }
    }

    fn handle_php_statement(&mut self, node: TsNode<'_>, source: &[u8]) {
        let mut cursor = node.walk();
        if !cursor.goto_first_child() {
            return;
        }
        loop {
            let child = cursor.node();
            if child.kind() == "php_only" {
                let text = node_text(child, source).trim().to_owned();
                if !text.is_empty() {
                    self.push_ref("blade_php_expression", text, span_from_node(child));
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    fn handle_element(&mut self, node: TsNode<'_>, source: &[u8]) {
        if let Some(tag) = component_tag_name(node, source) {
            let component_name = tag.trim_start_matches("x-").to_owned();
            self.push_ref("blade_x_component", component_name, span_from_node(node));
        }
    }

    fn handle_inline_directive(&mut self, node: TsNode<'_>, source: &[u8]) {
        let directive_text = node_text(node, source);
        let directive_name = directive_text.trim();
        let ref_kind = match directive_name {
            "@extends" | "@include" | "@includeIf" | "@includeWhen" | "@includeUnless"
            | "@includeFirst" | "@each" => "blade_view",
            "@yield" => "blade_section_ref",
            "@stack" => "blade_stack_ref",
            _ => return,
        };
        let Some(param_node) = next_parameter_sibling(node) else {
            return;
        };
        let param_text = node_text(param_node, source);
        let Some(name) = first_string_literal(&param_text) else {
            return;
        };
        let node_kind = match directive_name {
            "@yield" => Some("blade_yield"),
            "@stack" => Some("blade_stack"),
            _ => None,
        };
        if let Some(kind) = node_kind {
            self.push_node(kind, Some(name.clone()), span_from_node(node));
        }
        self.push_ref(ref_kind, name, span_from_node(param_node));
    }

    fn collect_diagnostics(&mut self, node: TsNode<'_>, source: &[u8]) {
        if !node.has_error() {
            return;
        }
        let mut cursor = node.walk();
        self.visit_for_errors(&mut cursor, source);
    }

    fn visit_for_errors(&mut self, cursor: &mut tree_sitter::TreeCursor<'_>, source: &[u8]) {
        let node = cursor.node();
        if node.is_missing() {
            let text = node_text(node, source);
            let label = if text.is_empty() {
                node.kind().to_owned()
            } else {
                text
            };
            self.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message: format!("missing token: {label}"),
                span: Some(span_from_node(node)),
            });
        } else if node.is_error() {
            let snippet = node_text(node, source);
            self.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "syntax error near: {}",
                    snippet.chars().take(40).collect::<String>()
                ),
                span: Some(span_from_node(node)),
            });
        }
        if cursor.goto_first_child() {
            loop {
                self.visit_for_errors(cursor, source);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            cursor.goto_parent();
        }
    }
}

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

fn block_directive_name(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        if matches!(child.kind(), "directive_start" | "directive") {
            return Some(node_text(child, source).trim().to_owned());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

fn first_parameter_node<'tree>(node: TsNode<'tree>) -> Option<TsNode<'tree>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        if cursor.node().kind() == "parameter" {
            return Some(cursor.node());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

fn first_parameter_text(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    first_parameter_node(node).map(|n| node_text(n, source))
}

fn next_parameter_sibling(node: TsNode<'_>) -> Option<TsNode<'_>> {
    let mut current = node.next_sibling();
    while let Some(sib) = current {
        match sib.kind() {
            "parameter" => return Some(sib),
            "(" => {
                current = sib.next_sibling();
                continue;
            }
            _ => return None,
        }
    }
    None
}

fn component_tag_name(node: TsNode<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        if matches!(child.kind(), "start_tag" | "self_closing_tag") {
            let mut inner = child.walk();
            if inner.goto_first_child() {
                loop {
                    let inner_node = inner.node();
                    if inner_node.kind() == "tag_name" {
                        let name = node_text(inner_node, source);
                        if name.starts_with("x-") {
                            return Some(name);
                        }
                        return None;
                    }
                    if !inner.goto_next_sibling() {
                        break;
                    }
                }
            }
            return None;
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

fn node_text(node: TsNode<'_>, source: &[u8]) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    if end <= source.len() && start <= end {
        String::from_utf8_lossy(&source[start..end]).into_owned()
    } else {
        String::new()
    }
}

fn first_string_literal(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let mut chars = trimmed.char_indices();
    let (start_idx, quote) = chars.next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let content_start = start_idx + quote.len_utf8();
    let after_quote = &trimmed[content_start..];
    let mut prev_was_backslash = false;
    for (idx, ch) in after_quote.char_indices() {
        if ch == quote && !prev_was_backslash {
            return Some(after_quote[..idx].to_owned());
        }
        prev_was_backslash = ch == '\\' && !prev_was_backslash;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Vec<u8> {
        let path: PathBuf = ["tests", "fixtures", "blade", name].iter().collect();
        fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn extract_bytes(source: &[u8]) -> ExtractedFile {
        BladePlugin.extract(source, Path::new("test.blade.php"))
    }

    fn find_ref<'a>(refs: &'a [Ref], kind: &str, name: &str) -> Option<&'a Ref> {
        refs.iter().find(|r| r.kind == kind && r.name == name)
    }

    #[test]
    fn language_constructs() {
        let lang = language();
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn parses_simple_directive() {
        let extracted = extract_bytes(b"@yield('content')\n");
        assert!(extracted.diagnostics.is_empty());
        assert!(find_ref(&extracted.refs, "blade_section_ref", "content").is_some());
    }

    #[test]
    fn extends_section_include_capture_view_refs() {
        let src = fixture("layout.blade.php");
        let extracted = extract_bytes(&src);
        assert!(
            extracted.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            extracted.diagnostics
        );
        assert!(
            find_ref(&extracted.refs, "blade_view", "layouts.app").is_some(),
            "missing @extends ref; refs={:?}",
            extracted.refs
        );
        assert!(
            find_ref(&extracted.refs, "blade_view", "partials.header").is_some(),
            "missing @include ref; refs={:?}",
            extracted.refs
        );
        assert!(
            find_ref(&extracted.refs, "blade_section_ref", "content").is_some(),
            "missing @section ref; refs={:?}",
            extracted.refs
        );
        assert!(
            extracted
                .nodes
                .iter()
                .any(|n| n.kind == "blade_section" && n.name == "content"),
            "expected Section node for content; nodes={:?}",
            extracted.nodes
        );
    }

    #[test]
    fn component_block_and_x_component_are_captured() {
        let src = fixture("components.blade.php");
        let extracted = extract_bytes(&src);
        assert!(
            extracted.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            extracted.diagnostics
        );
        assert!(
            find_ref(&extracted.refs, "blade_component", "alert").is_some(),
            "missing @component ref; refs={:?}",
            extracted.refs
        );
        assert!(
            find_ref(&extracted.refs, "blade_x_component", "card.body").is_some(),
            "missing <x-card.body> ref; refs={:?}",
            extracted.refs
        );
        assert!(
            extracted
                .nodes
                .iter()
                .any(|n| n.kind == "blade_component_block" && n.name == "alert"),
            "expected component block node; nodes={:?}",
            extracted.nodes
        );
    }

    #[test]
    fn embedded_php_expression_is_captured() {
        let src = fixture("expression.blade.php");
        let extracted = extract_bytes(&src);
        assert!(
            extracted.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            extracted.diagnostics
        );
        let php_ref = extracted
            .refs
            .iter()
            .find(|r| r.kind == "blade_php_expression" && r.name.contains("$user->name"))
            .unwrap_or_else(|| panic!("expected PhpExpression ref; refs={:?}", extracted.refs));
        assert!(
            php_ref.span.end.byte > php_ref.span.start.byte,
            "span must be non-empty"
        );
    }

    #[test]
    fn malformed_fixture_emits_diagnostic_and_partial_nodes() {
        let src = fixture("broken.blade.php");
        let extracted = extract_bytes(&src);
        assert!(
            !extracted.diagnostics.is_empty(),
            "expected at least one diagnostic"
        );
        assert!(
            !extracted.refs.is_empty() || !extracted.nodes.is_empty(),
            "expected partial extraction even with errors; nodes={:?} refs={:?}",
            extracted.nodes,
            extracted.refs
        );
    }

    #[test]
    fn push_directive_captures_stack_ref() {
        let extracted = extract_bytes(b"@push('scripts')\n<script>foo();</script>\n@endpush\n");
        assert!(find_ref(&extracted.refs, "blade_stack_ref", "scripts").is_some());
        assert!(
            extracted
                .nodes
                .iter()
                .any(|n| n.kind == "blade_push_block" && n.name == "scripts")
        );
    }

    #[test]
    fn first_string_literal_handles_quotes() {
        assert_eq!(first_string_literal("'foo'").as_deref(), Some("foo"));
        assert_eq!(first_string_literal("\"bar\"").as_deref(), Some("bar"));
        assert_eq!(
            first_string_literal(" 'name', 'default'").as_deref(),
            Some("name")
        );
        assert_eq!(first_string_literal("foo").as_deref(), None);
        assert_eq!(
            first_string_literal("'esc\\'aped'").as_deref(),
            Some("esc\\'aped")
        );
    }

    #[test]
    fn blade_plugin_metadata() {
        let plugin = BladePlugin;
        assert_eq!(plugin.id(), LanguageId::Blade);
        assert_eq!(plugin.extensions(), &["blade.php"]);
    }
}
