//! React framework resolver.
//!
//! Operates as a side layer over the JS / TS / TSX extractors, consuming
//! the canonical `ExtractedFile` and emitting framework edges that
//! classify which functions are React components, which are hooks, and
//! which call sites invoke React's built-in or user-defined hooks.
//!
//! Detection is purely syntactic — no type inference, no module
//! resolution — so it's fast and survives missing dependencies. Heuristics
//! are documented at each rule below; each emits a confidence level so
//! downstream tools can filter aggressive vs conservative classifications.
//!
//! Edges go to synthetic `lh:react.*` target node IDs so the same hot
//! index / Cozo query path used for Laravel works without further changes.

use crate::extract::ExtractedFile;
use crate::laravel::{Confidence, FrameworkEdge, FrameworkEdgeKind, LaravelFacts, Provenance};

/// React's built-in hooks. Order-independent — used for membership
/// checks only. Listed verbatim from React 19; extended only when a new
/// hook ships in stable React. Kept as a sorted array so callers can
/// binary-search if profiling shows the linear scan is hot.
pub const BUILTIN_HOOKS: &[&str] = &[
    "useActionState",
    "useCallback",
    "useContext",
    "useDebugValue",
    "useDeferredValue",
    "useEffect",
    "useFormStatus",
    "useId",
    "useImperativeHandle",
    "useInsertionEffect",
    "useLayoutEffect",
    "useMemo",
    "useOptimistic",
    "useReducer",
    "useRef",
    "useState",
    "useSyncExternalStore",
    "useTransition",
];

/// Resolve React framework facts from a batch of JS / TS / TSX extracted
/// files. Returns the same `LaravelFacts` shape that the Laravel resolver
/// produces (since framework edges are reused across resolvers) and
/// extends it with React-specific edge kinds.
pub fn resolve_react(inputs: &[&ExtractedFile]) -> LaravelFacts {
    let mut facts = LaravelFacts::default();
    for file in inputs {
        classify_file(file, &mut facts);
    }
    facts
}

fn classify_file(file: &ExtractedFile, facts: &mut LaravelFacts) {
    // Component-or-hook? Walk nodes once, looking only at top-level
    // function/variable/method definitions whose name passes the
    // naming gate. Index the file's refs by container so we can decide
    // "does this node contain JSX / hook calls" in O(1) per node.
    let refs_by_container = group_refs_by_container(file);

    let empty: Vec<&crate::extract::Ref> = Vec::new();
    for node in &file.nodes {
        // We only classify function-like top-level definitions. Skip
        // nested defs (parent is set), classes (they may be React class
        // components but are detected separately below), constants, etc.
        let is_function_like = matches!(
            node.kind.as_str(),
            "function" | "arrow_function" | "variable" | "method"
        );
        if !is_function_like {
            continue;
        }
        let container_refs: &[&crate::extract::Ref] =
            refs_by_container.get(&node.id).unwrap_or(&empty);

        if is_custom_hook_name(&node.name) {
            facts.edges.push(FrameworkEdge {
                from_qname: node.qname.clone(),
                to_qname: "react.hook".to_owned(),
                kind: FrameworkEdgeKind::ReactHook,
                provenance: Provenance::LaravelHeuristic,
                confidence: Confidence::High,
            });
            emit_hook_call_edges(node, container_refs, facts);
            continue;
        }
        if is_component_name(&node.name) && contains_jsx(container_refs) {
            facts.edges.push(FrameworkEdge {
                from_qname: node.qname.clone(),
                to_qname: "react.component".to_owned(),
                kind: FrameworkEdgeKind::ReactComponent,
                provenance: Provenance::LaravelHeuristic,
                confidence: Confidence::High,
            });
            emit_hook_call_edges(node, container_refs, facts);
        }
    }

    // Class components: `class Foo extends React.Component` or
    // `extends Component`. The inheritance ref already encodes the
    // superclass name; we look for it among the file's refs.
    classify_class_components(file, facts);

    // Wrapper patterns: `memo(Foo)` / `forwardRef(Foo)` at module
    // scope. These are call_expressions whose container is the
    // top-level variable they bind to.
    classify_wrapper_calls(file, facts);
}

fn classify_class_components(file: &ExtractedFile, facts: &mut LaravelFacts) {
    for r in &file.refs {
        if r.kind != "inheritance" && r.kind != "extends" {
            continue;
        }
        if r.name != "Component"
            && r.name != "PureComponent"
            && r.name != "React.Component"
            && r.name != "React.PureComponent"
        {
            continue;
        }
        // Find the class node that owns this inheritance ref.
        let Some(container_id) = r.container else {
            continue;
        };
        let Some(class_node) = file.nodes.iter().find(|n| n.id == container_id) else {
            continue;
        };
        if class_node.kind != "class" {
            continue;
        }
        facts.edges.push(FrameworkEdge {
            from_qname: class_node.qname.clone(),
            to_qname: "react.component".to_owned(),
            kind: FrameworkEdgeKind::ReactComponent,
            provenance: Provenance::LaravelHeuristic,
            confidence: Confidence::High,
        });
    }
}

fn classify_wrapper_calls(file: &ExtractedFile, facts: &mut LaravelFacts) {
    for r in &file.refs {
        if r.kind != "call" {
            continue;
        }
        let is_wrapper = matches!(
            r.name.as_str(),
            "memo" | "forwardRef" | "React.memo" | "React.forwardRef"
        );
        if !is_wrapper {
            continue;
        }
        let Some(container_id) = r.container else {
            continue;
        };
        let Some(wrapper_node) = file.nodes.iter().find(|n| n.id == container_id) else {
            continue;
        };
        // Only emit when the wrapping happens at top-level binding
        // scope. Inner `memo(Inner)` invocations are unusual and not
        // typically used to declare a stable component identity.
        if wrapper_node.parent.is_some() {
            continue;
        }
        facts.edges.push(FrameworkEdge {
            from_qname: wrapper_node.qname.clone(),
            to_qname: "react.component".to_owned(),
            kind: FrameworkEdgeKind::ReactComponent,
            provenance: Provenance::LaravelHeuristic,
            confidence: Confidence::Medium,
        });
    }
}

fn emit_hook_call_edges(
    node: &crate::extract::Node,
    container_refs: &[&crate::extract::Ref],
    facts: &mut LaravelFacts,
) {
    for r in container_refs {
        if r.kind != "call" {
            continue;
        }
        if is_builtin_hook(&r.name) {
            facts.edges.push(FrameworkEdge {
                from_qname: node.qname.clone(),
                to_qname: format!("react.hook.{}", r.name),
                kind: FrameworkEdgeKind::ReactUsesHook,
                provenance: Provenance::LaravelHeuristic,
                confidence: Confidence::High,
            });
        } else if is_custom_hook_name(&r.name) {
            facts.edges.push(FrameworkEdge {
                from_qname: node.qname.clone(),
                to_qname: format!("react.hook.{}", r.name),
                kind: FrameworkEdgeKind::ReactUsesHook,
                provenance: Provenance::LaravelHeuristic,
                confidence: Confidence::Medium,
            });
        }
    }
}

/// Group every `Ref` in the file by its `container` node id so callers
/// can ask "does node X contain JSX / a hook call" without re-scanning
/// the whole ref list per node.
///
/// Borrow shape is intentional: we return a `BTreeMap<u32, &[&Ref]>`-like
/// where the value slices into a small backing arena. For tiny files
/// (most files) the linear scan would be just as fast, but for files
/// with 1000+ refs the per-node fan-out adds up.
fn group_refs_by_container(
    file: &ExtractedFile,
) -> std::collections::HashMap<crate::extract::LocalNodeId, Vec<&crate::extract::Ref>> {
    let mut map: std::collections::HashMap<_, Vec<&crate::extract::Ref>> =
        std::collections::HashMap::with_capacity(file.nodes.len());
    for r in &file.refs {
        if let Some(c) = r.container {
            map.entry(c).or_default().push(r);
        }
    }
    map
}

fn contains_jsx(refs: &[&crate::extract::Ref]) -> bool {
    refs.iter()
        .any(|r| r.kind == "jsx_component" || r.kind == "jsx_element")
}

/// `Foo`, `MyComponent`, `_Foo` (leading underscore tolerated). Excludes
/// `foo`, `f`, names with non-letter first char.
pub fn is_component_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => true,
        Some('_') => matches!(chars.next(), Some(c) if c.is_ascii_uppercase()),
        _ => false,
    }
}

/// `useFoo`, `useThing`. Rejects `use`, `used`, `users`, lowercase
/// continuations. Mirrors React's official lint rule.
pub fn is_custom_hook_name(name: &str) -> bool {
    name.len() >= 4
        && name.starts_with("use")
        && name.chars().nth(3).is_some_and(|c| c.is_ascii_uppercase())
}

pub fn is_builtin_hook(name: &str) -> bool {
    BUILTIN_HOOKS.binary_search(&name).is_ok()
}

// We extend `FrameworkEdgeKind` (defined in `laravel.rs`) to carry React
// kinds. Helper here returns the existing variants the resolver emits so
// the writer-side label table stays close to the data.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{LocalNodeId, Node, Position, Ref, Span};
    use std::path::PathBuf;

    fn span(start: usize, end: usize) -> Span {
        Span {
            start: Position {
                byte: start,
                row: 0,
                column: 0,
            },
            end: Position {
                byte: end,
                row: 0,
                column: 0,
            },
        }
    }

    fn function_node(id: LocalNodeId, name: &str, span: Span) -> Node {
        Node {
            id,
            kind: "function".to_string(),
            name: name.to_string(),
            qname: name.to_string(),
            span,
            parent: None,
        }
    }

    fn class_node(id: LocalNodeId, name: &str, span: Span) -> Node {
        Node {
            id,
            kind: "class".to_string(),
            name: name.to_string(),
            qname: name.to_string(),
            span,
            parent: None,
        }
    }

    fn ref_for(id: u32, kind: &str, name: &str, container: Option<LocalNodeId>, span: Span) -> Ref {
        Ref {
            id,
            kind: kind.to_string(),
            name: name.to_string(),
            qname: None,
            alias: None,
            span,
            container,
        }
    }

    fn file_with(nodes: Vec<Node>, refs: Vec<Ref>) -> ExtractedFile {
        ExtractedFile {
            path: PathBuf::from("Component.tsx"),
            nodes,
            refs,
            diagnostics: vec![],
        }
    }

    #[test]
    fn component_name_predicate() {
        assert!(is_component_name("Foo"));
        assert!(is_component_name("MyComponent"));
        assert!(is_component_name("_Internal"));
        assert!(!is_component_name("foo"));
        assert!(!is_component_name("doSomething"));
        assert!(!is_component_name(""));
    }

    #[test]
    fn custom_hook_name_predicate() {
        assert!(is_custom_hook_name("useState"));
        assert!(is_custom_hook_name("useThing"));
        assert!(!is_custom_hook_name("use"));
        assert!(!is_custom_hook_name("used"));
        assert!(!is_custom_hook_name("useradd"));
        assert!(!is_custom_hook_name("Foo"));
    }

    #[test]
    fn builtin_hook_recognition() {
        assert!(is_builtin_hook("useState"));
        assert!(is_builtin_hook("useEffect"));
        assert!(!is_builtin_hook("useThing"));
        assert!(!is_builtin_hook("foo"));
    }

    #[test]
    fn function_returning_jsx_is_classified_as_component() {
        let file = file_with(
            vec![function_node(0, "MyButton", span(0, 100))],
            vec![ref_for(0, "jsx_component", "div", Some(0), span(20, 30))],
        );
        let facts = resolve_react(&[&file]);
        assert!(
            facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::ReactComponent && e.from_qname == "MyButton")
        );
    }

    #[test]
    fn lowercase_function_with_jsx_is_not_component() {
        let file = file_with(
            vec![function_node(0, "helper", span(0, 100))],
            vec![ref_for(0, "jsx_component", "div", Some(0), span(20, 30))],
        );
        let facts = resolve_react(&[&file]);
        assert!(
            !facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::ReactComponent)
        );
    }

    #[test]
    fn pascal_case_function_without_jsx_is_not_component() {
        let file = file_with(vec![function_node(0, "Foo", span(0, 100))], vec![]);
        let facts = resolve_react(&[&file]);
        assert!(
            !facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::ReactComponent),
            "no JSX inside → not a component"
        );
    }

    #[test]
    fn function_named_use_x_is_hook_even_without_calls() {
        let file = file_with(vec![function_node(0, "useThing", span(0, 100))], vec![]);
        let facts = resolve_react(&[&file]);
        assert!(
            facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::ReactHook && e.from_qname == "useThing")
        );
    }

    #[test]
    fn component_using_builtin_hook_emits_uses_hook_edge() {
        let file = file_with(
            vec![function_node(0, "MyComp", span(0, 100))],
            vec![
                ref_for(0, "jsx_component", "div", Some(0), span(50, 60)),
                ref_for(1, "call", "useState", Some(0), span(20, 30)),
            ],
        );
        let facts = resolve_react(&[&file]);
        assert!(
            facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::ReactUsesHook
                    && e.from_qname == "MyComp"
                    && e.to_qname == "react.hook.useState")
        );
    }

    #[test]
    fn class_extending_react_component_is_classified() {
        let file = file_with(
            vec![class_node(0, "OldStyle", span(0, 200))],
            vec![ref_for(
                0,
                "inheritance",
                "Component",
                Some(0),
                span(10, 20),
            )],
        );
        let facts = resolve_react(&[&file]);
        assert!(
            facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::ReactComponent && e.from_qname == "OldStyle")
        );
    }

    #[test]
    fn memo_wrapper_call_classifies_wrapper_as_component() {
        // `const Wrapped = memo(Inner);` — wrapper is `Wrapped`, the
        // `memo` call lives inside its variable_declarator.
        let mut nodes = vec![Node {
            id: 0,
            kind: "variable".to_string(),
            name: "Wrapped".to_string(),
            qname: "Wrapped".to_string(),
            span: span(0, 60),
            parent: None,
        }];
        // The inner component also extracted.
        nodes.push(function_node(1, "Inner", span(70, 200)));
        let refs = vec![ref_for(0, "call", "memo", Some(0), span(10, 50))];
        let file = file_with(nodes, refs);
        let facts = resolve_react(&[&file]);
        assert!(
            facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::ReactComponent && e.from_qname == "Wrapped")
        );
    }
}
