use std::ops::Range;

use tree_sitter::{Language, Node, Parser, Tree};

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

pub fn language() -> Language {
    ffi::language()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Section,
    Yield,
    Stack,
    PushBlock,
    PrependBlock,
    PhpBlock,
    BladeComponentBlock,
    DirectiveBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    BladeView,
    BladeSection,
    BladeStack,
    BladeComponent,
    BladeXComponent,
    PhpExpression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

impl Span {
    fn from_node(node: Node<'_>) -> Self {
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
    pub name: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedRef {
    pub kind: RefKind,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EmbeddedRange {
    pub language: EmbeddedLanguage,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedLanguage {
    Php,
    JavaScript,
    Css,
}

#[derive(Debug, Default, Clone)]
pub struct ExtractedFile {
    pub nodes: Vec<ExtractedNode>,
    pub refs: Vec<ExtractedRef>,
    pub diagnostics: Vec<Diagnostic>,
    pub embedded: Vec<EmbeddedRange>,
}

pub trait LanguagePlugin {
    fn id(&self) -> &'static str;
    fn extensions(&self) -> &'static [&'static str];
    fn tree_sitter_language(&self) -> Language;
    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile;
}

pub struct BladePlugin;

impl LanguagePlugin for BladePlugin {
    fn id(&self) -> &'static str {
        "blade"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["blade.php"]
    }

    fn tree_sitter_language(&self) -> Language {
        language()
    }

    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile {
        let mut out = ExtractedFile::default();
        let root = tree.root_node();
        walk(root, source, &mut out);
        collect_diagnostics(root, source, &mut out.diagnostics);
        out
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

pub fn parse(source: &[u8]) -> Result<Tree, ParseError> {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .map_err(|e| ParseError(format!("set_language: {e}")))?;
    parser
        .parse(source, None)
        .ok_or_else(|| ParseError("parser returned no tree".to_owned()))
}

pub fn extract(source: &[u8]) -> Result<ExtractedFile, ParseError> {
    let tree = parse(source)?;
    Ok(BladePlugin.extract(&tree, source))
}

fn walk(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    match node.kind() {
        "section" => handle_section(node, source, out),
        "stack" => handle_stack(node, source, out),
        "conditional" => handle_conditional(node, source, out),
        "php_statement" => handle_php_statement(node, source, out),
        "element" => handle_element(node, source, out),
        "directive" => handle_inline_directive(node, source, out),
        _ => {}
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk(cursor.node(), source, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn handle_section(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    let directive_name = block_directive_name(node, source);
    let parameter_text = first_parameter_text(node, source);
    let section_name = parameter_text.as_deref().and_then(first_string_literal);

    out.nodes.push(ExtractedNode {
        kind: NodeKind::Section,
        name: section_name.clone(),
        span: Span::from_node(node),
    });

    if directive_name.as_deref() == Some("@section")
        && let Some(name) = section_name
        && let Some(param_node) = first_parameter_node(node)
    {
        out.refs.push(ExtractedRef {
            kind: RefKind::BladeSection,
            name,
            span: Span::from_node(param_node),
        });
    }
}

fn handle_stack(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    let directive_name = block_directive_name(node, source);
    let kind = match directive_name.as_deref() {
        Some("@prepend" | "@prependOnce") => NodeKind::PrependBlock,
        _ => NodeKind::PushBlock,
    };
    let parameter_text = first_parameter_text(node, source);
    let stack_name = parameter_text.as_deref().and_then(first_string_literal);
    out.nodes.push(ExtractedNode {
        kind,
        name: stack_name.clone(),
        span: Span::from_node(node),
    });
    if let Some(name) = stack_name
        && let Some(param_node) = first_parameter_node(node)
    {
        out.refs.push(ExtractedRef {
            kind: RefKind::BladeStack,
            name,
            span: Span::from_node(param_node),
        });
    }
}

fn handle_conditional(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    let Some(name) = block_directive_name(node, source) else {
        return;
    };
    match name.as_str() {
        "@component" => emit_component(node, source, out),
        "@php" => out.nodes.push(ExtractedNode {
            kind: NodeKind::PhpBlock,
            name: None,
            span: Span::from_node(node),
        }),
        _ => out.nodes.push(ExtractedNode {
            kind: NodeKind::DirectiveBlock,
            name: Some(name),
            span: Span::from_node(node),
        }),
    }
}

fn emit_component(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    let parameter_text = first_parameter_text(node, source);
    let component_name = parameter_text.as_deref().and_then(first_string_literal);
    out.nodes.push(ExtractedNode {
        kind: NodeKind::BladeComponentBlock,
        name: component_name.clone(),
        span: Span::from_node(node),
    });
    if let Some(name) = component_name
        && let Some(param_node) = first_parameter_node(node)
    {
        out.refs.push(ExtractedRef {
            kind: RefKind::BladeComponent,
            name,
            span: Span::from_node(param_node),
        });
    }
}

fn handle_php_statement(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let child = cursor.node();
        if child.kind() == "php_only" {
            let span = Span::from_node(child);
            let text = node_text(child, source).trim().to_owned();
            if !text.is_empty() {
                out.refs.push(ExtractedRef {
                    kind: RefKind::PhpExpression,
                    name: text,
                    span: span.clone(),
                });
            }
            out.embedded.push(EmbeddedRange {
                language: EmbeddedLanguage::Php,
                range: span.start_byte..span.end_byte,
            });
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn handle_element(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    if let Some(tag) = component_tag_name(node, source) {
        let component_name = tag.trim_start_matches("x-").to_owned();
        out.refs.push(ExtractedRef {
            kind: RefKind::BladeXComponent,
            name: component_name,
            span: Span::from_node(node),
        });
        return;
    }
    let mut child_walker = node.walk();
    if child_walker.goto_first_child() {
        loop {
            let child = child_walker.node();
            match child.kind() {
                "script_element" => {
                    collect_embedded(child, EmbeddedLanguage::JavaScript, &mut out.embedded)
                }
                "style_element" => {
                    collect_embedded(child, EmbeddedLanguage::Css, &mut out.embedded)
                }
                _ => {}
            }
            if !child_walker.goto_next_sibling() {
                break;
            }
        }
    }
}

fn handle_inline_directive(node: Node<'_>, source: &[u8], out: &mut ExtractedFile) {
    let directive_text = node_text(node, source);
    let directive_name = directive_text.trim();
    let ref_kind = match directive_name {
        "@extends" | "@include" | "@includeIf" | "@includeWhen" | "@includeUnless"
        | "@includeFirst" | "@each" => RefKind::BladeView,
        "@yield" => RefKind::BladeSection,
        "@stack" => RefKind::BladeStack,
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
        "@yield" => Some(NodeKind::Yield),
        "@stack" => Some(NodeKind::Stack),
        _ => None,
    };
    if let Some(kind) = node_kind {
        out.nodes.push(ExtractedNode {
            kind,
            name: Some(name.clone()),
            span: Span::from_node(node),
        });
    }
    out.refs.push(ExtractedRef {
        kind: ref_kind,
        name,
        span: Span::from_node(param_node),
    });
}

fn collect_embedded(element: Node<'_>, language: EmbeddedLanguage, out: &mut Vec<EmbeddedRange>) {
    let mut cursor = element.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let child = cursor.node();
        if child.kind() == "raw_text" {
            out.push(EmbeddedRange {
                language,
                range: child.start_byte()..child.end_byte(),
            });
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn collect_diagnostics(node: Node<'_>, source: &[u8], out: &mut Vec<Diagnostic>) {
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    visit_for_errors(&mut cursor, source, out);
}

fn visit_for_errors(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    source: &[u8],
    out: &mut Vec<Diagnostic>,
) {
    let node = cursor.node();
    if node.is_missing() {
        let text = node_text(node, source);
        let label = if text.is_empty() {
            node.kind().to_owned()
        } else {
            text
        };
        out.push(Diagnostic {
            message: format!("missing token: {label}"),
            span: Span::from_node(node),
        });
    } else if node.is_error() {
        let snippet = node_text(node, source);
        out.push(Diagnostic {
            message: format!(
                "syntax error near: {}",
                snippet.chars().take(40).collect::<String>()
            ),
            span: Span::from_node(node),
        });
    }
    if cursor.goto_first_child() {
        loop {
            visit_for_errors(cursor, source, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn block_directive_name(node: Node<'_>, source: &[u8]) -> Option<String> {
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

fn first_parameter_node<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
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

fn first_parameter_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    first_parameter_node(node).map(|n| node_text(n, source))
}

fn next_parameter_sibling(node: Node<'_>) -> Option<Node<'_>> {
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

fn component_tag_name(node: Node<'_>, source: &[u8]) -> Option<String> {
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

fn node_text(node: Node<'_>, source: &[u8]) -> String {
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

    fn find_ref<'a>(
        refs: &'a [ExtractedRef],
        kind: RefKind,
        name: &str,
    ) -> Option<&'a ExtractedRef> {
        refs.iter().find(|r| r.kind == kind && r.name == name)
    }

    #[test]
    fn language_constructs() {
        let lang = language();
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn parses_simple_directive() {
        let src = b"@yield('content')\n";
        let extracted = extract(src).expect("parse");
        assert!(extracted.diagnostics.is_empty());
        assert!(find_ref(&extracted.refs, RefKind::BladeSection, "content").is_some());
    }

    #[test]
    fn extends_section_include_capture_view_refs() {
        let src = fixture("layout.blade.php");
        let extracted = extract(&src).expect("parse");
        assert!(
            extracted.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            extracted.diagnostics
        );
        assert!(
            find_ref(&extracted.refs, RefKind::BladeView, "layouts.app").is_some(),
            "missing @extends ref; refs={:?}",
            extracted.refs
        );
        assert!(
            find_ref(&extracted.refs, RefKind::BladeView, "partials.header").is_some(),
            "missing @include ref; refs={:?}",
            extracted.refs
        );
        assert!(
            find_ref(&extracted.refs, RefKind::BladeSection, "content").is_some(),
            "missing @section ref; refs={:?}",
            extracted.refs
        );
        assert!(
            extracted
                .nodes
                .iter()
                .any(|n| n.kind == NodeKind::Section && n.name.as_deref() == Some("content")),
            "expected Section node for content; nodes={:?}",
            extracted.nodes
        );
    }

    #[test]
    fn component_block_and_x_component_are_captured() {
        let src = fixture("components.blade.php");
        let extracted = extract(&src).expect("parse");
        assert!(
            extracted.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            extracted.diagnostics
        );
        assert!(
            find_ref(&extracted.refs, RefKind::BladeComponent, "alert").is_some(),
            "missing @component ref; refs={:?}",
            extracted.refs
        );
        assert!(
            find_ref(&extracted.refs, RefKind::BladeXComponent, "card.body").is_some(),
            "missing <x-card.body> ref; refs={:?}",
            extracted.refs
        );
        assert!(
            extracted
                .nodes
                .iter()
                .any(|n| n.kind == NodeKind::BladeComponentBlock
                    && n.name.as_deref() == Some("alert")),
            "expected component block node; nodes={:?}",
            extracted.nodes
        );
    }

    #[test]
    fn embedded_php_expression_is_captured() {
        let src = fixture("expression.blade.php");
        let extracted = extract(&src).expect("parse");
        assert!(
            extracted.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            extracted.diagnostics
        );
        let php_ref = extracted
            .refs
            .iter()
            .find(|r| r.kind == RefKind::PhpExpression && r.name.contains("$user->name"))
            .unwrap_or_else(|| panic!("expected PhpExpression ref; refs={:?}", extracted.refs));
        assert!(
            php_ref.span.end_byte > php_ref.span.start_byte,
            "span must be non-empty"
        );
        assert!(
            extracted
                .embedded
                .iter()
                .any(|e| e.language == EmbeddedLanguage::Php),
            "expected embedded PHP range; embedded={:?}",
            extracted.embedded
        );
    }

    #[test]
    fn malformed_fixture_emits_diagnostic_and_partial_nodes() {
        let src = fixture("broken.blade.php");
        let extracted = extract(&src).expect("parse");
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
        let src = b"@push('scripts')\n<script>foo();</script>\n@endpush\n";
        let extracted = extract(src).expect("parse");
        assert!(find_ref(&extracted.refs, RefKind::BladeStack, "scripts").is_some());
        assert!(
            extracted
                .nodes
                .iter()
                .any(|n| n.kind == NodeKind::PushBlock && n.name.as_deref() == Some("scripts"))
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
        assert_eq!(plugin.id(), "blade");
        assert_eq!(plugin.extensions(), &["blade.php"]);
    }
}
