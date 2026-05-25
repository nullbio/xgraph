//! Laravel framework resolver.
//!
//! Operates as a separate layer over generic PHP extraction. Inputs are
//! arrays of [`PhpExtractInput`] produced by the PHP extractor (modelled
//! locally here so this unit is testable in isolation). Outputs are
//! [`LaravelFacts`] carrying framework edges, view references, and
//! diagnostics for unresolved patterns. Every emitted edge is tagged with
//! [`Provenance::LaravelHeuristic`] and a documented [`Confidence`].

use std::path::PathBuf;

/// Byte span within a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Source-level node kind sufficient to model containers Laravel cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhpNodeKind {
    Class,
    Method,
    Function,
}

/// A PHP definition extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpNode {
    pub kind: PhpNodeKind,
    pub name: String,
    pub qname: String,
    pub span: Span,
}

/// A literal argument in a PHP call expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhpArg {
    /// A quoted string literal value (already unescaped/unquoted).
    String(String),
    /// A `Foo::class` constant reference.
    ClassConstant(String),
    /// An array literal containing argument elements in declaration order.
    Array(Vec<PhpArg>),
    /// A `new Foo(...)` expression carrying the class name.
    NewInstance(String),
    /// An arbitrary expression Laravel resolution does not interpret.
    Other,
}

/// Receiver of a PHP call expression, mirroring the shape Laravel patterns
/// rely on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhpCallReceiver {
    /// A plain function call (no receiver), e.g. `event(...)`.
    None,
    /// A static call on a bare identifier, e.g. `Route::get(...)`.
    StaticClass(String),
    /// A method call on `$this`, e.g. `$this->hasMany(...)`.
    ThisInstance,
    /// A method call on `$this->app`, used for container bindings.
    ThisAppContainer,
    /// A method call on some other variable or expression.
    OtherInstance,
}

/// A PHP reference extracted from a source file. The fields are exactly what
/// the Laravel resolver needs to recognise framework patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpRef {
    /// Receiver shape for the call expression.
    pub receiver: PhpCallReceiver,
    /// Method or function name being invoked.
    pub method: String,
    /// Argument list in declaration order.
    pub args: Vec<PhpArg>,
    /// Span of the call expression in the source file.
    pub span: Span,
    /// Fully qualified name of the enclosing definition (class method,
    /// function, or top-level scope). Used to attribute resulting edges.
    pub enclosing_qname: String,
}

/// Per-file input bundle for the Laravel resolver. The resolver does not
/// inspect file content; it relies on extracted nodes and refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpExtractInput {
    pub path: PathBuf,
    pub nodes: Vec<PhpNode>,
    pub refs: Vec<PhpRef>,
}

/// Provenance tag for any framework-derived edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    LaravelHeuristic,
}

/// Confidence levels documented per pattern below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// Kinds of edge the Laravel resolver may emit. (Also used by the React
/// resolver — see [`crate::react`] — since both flow through the same
/// `lh:`-prefixed framework-edge synthesis path in the owner.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameworkEdgeKind {
    RouteToController,
    ControllerToModel,
    EloquentRelationship,
    FacadeCall,
    ServiceBinding,
    EventListener,
    JobDispatch,
    /// `@extends('layouts.app')` in a Blade template.
    BladeExtendsView,
    /// `@include`, `@includeIf`, `@each`, ... in a Blade template.
    BladeIncludesView,
    /// `@component('alert')` or `<x-alert />` in a Blade template.
    BladeUsesComponent,
    /// Function or class is a React component (function returns JSX, or
    /// class extends `Component`/`PureComponent`, or wrapped via
    /// `memo`/`forwardRef`).
    ReactComponent,
    /// Function whose name matches `use[A-Z]...` — a custom React hook.
    ReactHook,
    /// A component or hook calls another React hook (builtin or custom).
    ReactUsesHook,
}

/// A framework edge with mandatory provenance and confidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkEdge {
    pub from_qname: String,
    pub to_qname: String,
    pub kind: FrameworkEdgeKind,
    pub provenance: Provenance,
    pub confidence: Confidence,
}

/// A Blade view reference discovered through Laravel controllers/helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRef {
    pub view_name: String,
    pub from_qname: String,
}

/// Output of [`resolve`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaravelFacts {
    pub edges: Vec<FrameworkEdge>,
    pub view_refs: Vec<ViewRef>,
    pub diagnostics: Vec<String>,
}

/// Receivers recognised as Laravel facades.
const FACADE_NAMES: &[&str] = &[
    "Cache", "Log", "DB", "Auth", "Mail", "Queue", "Storage", "Session", "Redirect", "View",
    "Request", "Response", "Event",
];

/// HTTP verb methods on the `Route` facade whose first positional argument is
/// the route path and second is the action.
const ROUTE_VERB_METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "options", "any"];

/// Multi-verb route methods on the `Route` facade whose first positional
/// argument is the verb list and whose action argument is third.
const ROUTE_MATCH_METHODS: &[&str] = &["match"];

/// Static methods on a model class that signal a controller-to-model query.
const ELOQUENT_QUERY_METHODS: &[&str] = &[
    "find",
    "findOrFail",
    "findMany",
    "first",
    "firstOrFail",
    "where",
    "all",
    "create",
    "make",
    "updateOrCreate",
    "firstOrCreate",
    "firstOrNew",
];

/// Methods on `$this` that declare Eloquent relationships.
const ELOQUENT_RELATIONSHIPS: &[&str] = &[
    "hasOne",
    "hasMany",
    "belongsTo",
    "belongsToMany",
    "morphTo",
    "morphOne",
    "morphMany",
    "morphToMany",
    "hasManyThrough",
    "hasOneThrough",
];

/// Container binding methods on `$this->app`.
const CONTAINER_BINDINGS: &[&str] = &["bind", "singleton", "instance", "scoped"];

/// Resolve Laravel framework facts from the provided PHP extraction inputs.
pub fn resolve(inputs: &[PhpExtractInput]) -> LaravelFacts {
    let mut facts = LaravelFacts::default();

    for input in inputs {
        for php_ref in &input.refs {
            classify(php_ref, &mut facts);
        }
    }

    facts
}

/// Kinds of reference a Blade template surfaces to the framework resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BladeRefKind {
    /// `@extends('layouts.app')` — the template inherits from another view.
    ExtendsView,
    /// `@include('partials.header')` and friends — the template embeds
    /// another view at render time.
    IncludesView,
    /// `@component('alert')` — explicit component invocation.
    Component,
    /// `<x-alert />` — anonymous component or class component invocation.
    XComponent,
}

/// A single reference extracted from a Blade template that the framework
/// resolver may interpret. `value` carries the literal payload (view name,
/// component name) as written in the template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BladeRef {
    pub kind: BladeRefKind,
    pub value: String,
    pub span: Span,
}

/// Per-file input bundle for Blade-driven framework resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BladeExtractInput {
    /// Worktree-relative path of the template (e.g.
    /// `resources/views/users/index.blade.php`).
    pub path: PathBuf,
    pub refs: Vec<BladeRef>,
}

/// Resolve Laravel framework facts from Blade templates. Each template
/// becomes a `view.<dotted>` node and emits one framework edge per Blade
/// ref that names another view or component. Confidence is `High` because
/// Blade syntax leaves no ambiguity about what's being referenced.
pub fn resolve_blade(inputs: &[BladeExtractInput]) -> LaravelFacts {
    let mut facts = LaravelFacts::default();
    for input in inputs {
        let Some(from_qname) = blade_view_qname_from_path(&input.path) else {
            continue;
        };
        for r in &input.refs {
            let (kind, to_qname) = match r.kind {
                BladeRefKind::ExtendsView => (
                    FrameworkEdgeKind::BladeExtendsView,
                    format!("view.{}", r.value),
                ),
                BladeRefKind::IncludesView => (
                    FrameworkEdgeKind::BladeIncludesView,
                    format!("view.{}", r.value),
                ),
                BladeRefKind::Component | BladeRefKind::XComponent => (
                    FrameworkEdgeKind::BladeUsesComponent,
                    format!("component.{}", r.value),
                ),
            };
            facts.edges.push(FrameworkEdge {
                from_qname: from_qname.clone(),
                to_qname,
                kind,
                provenance: Provenance::LaravelHeuristic,
                confidence: Confidence::High,
            });
        }
    }
    facts
}

/// Map a worktree-relative Blade template path to its canonical view qname:
/// `resources/views/users/index.blade.php` → `view.users.index`.
/// Returns `None` for files outside the standard `resources/views/` tree.
pub fn blade_view_qname_from_path(path: &std::path::Path) -> Option<String> {
    let mut comps = path.components();
    // Look for the `resources/views/` prefix; the dotted name is the
    // remainder with the `.blade.php` extension stripped.
    let mut found_resources = false;
    let mut found_views = false;
    let mut tail: Vec<String> = Vec::new();
    for comp in comps.by_ref() {
        let part = comp.as_os_str().to_str()?;
        match (found_resources, found_views) {
            (false, _) if part == "resources" => found_resources = true,
            (true, false) if part == "views" => found_views = true,
            (true, true) => tail.push(part.to_owned()),
            _ => {
                // Reset if `resources` appeared mid-path without a following `views`.
                if !found_views {
                    found_resources = false;
                }
            }
        }
    }
    if !found_views || tail.is_empty() {
        return None;
    }
    let last_idx = tail.len() - 1;
    let last = &tail[last_idx];
    let stripped = last
        .strip_suffix(".blade.php")
        .or_else(|| last.strip_suffix(".php"))
        .unwrap_or(last);
    let mut dotted = tail[..last_idx].join(".");
    if !dotted.is_empty() && !stripped.is_empty() {
        dotted.push('.');
    }
    dotted.push_str(stripped);
    Some(format!("view.{dotted}"))
}

fn classify(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    match &php_ref.receiver {
        PhpCallReceiver::StaticClass(class_name) => {
            classify_static(class_name, php_ref, facts);
        }
        PhpCallReceiver::ThisInstance => {
            classify_this(php_ref, facts);
        }
        PhpCallReceiver::ThisAppContainer => {
            classify_container_binding(php_ref, facts);
        }
        PhpCallReceiver::None => {
            classify_function(php_ref, facts);
        }
        PhpCallReceiver::OtherInstance => {
            // Other receivers are not classified by the Laravel resolver;
            // they remain in generic PHP facts.
        }
    }
}

fn classify_static(class_name: &str, php_ref: &PhpRef, facts: &mut LaravelFacts) {
    if class_name == "Route" && ROUTE_VERB_METHODS.contains(&php_ref.method.as_str()) {
        handle_route_definition(php_ref, facts, RouteSignature::Verb);
        return;
    }

    if class_name == "Route" && ROUTE_MATCH_METHODS.contains(&php_ref.method.as_str()) {
        handle_route_definition(php_ref, facts, RouteSignature::Match);
        return;
    }

    if class_name == "View" && php_ref.method == "make" {
        handle_view_make(php_ref, facts);
        return;
    }

    if class_name == "Event" && (php_ref.method == "dispatch" || php_ref.method == "fire") {
        handle_event_dispatch(php_ref, facts);
        return;
    }

    if class_name == "Queue" && php_ref.method == "push" {
        handle_job_dispatch(php_ref, facts);
        return;
    }

    if is_facade_name(class_name) {
        facts.edges.push(FrameworkEdge {
            from_qname: php_ref.enclosing_qname.clone(),
            to_qname: format!("{}::{}", class_name, php_ref.method),
            kind: FrameworkEdgeKind::FacadeCall,
            provenance: Provenance::LaravelHeuristic,
            confidence: Confidence::High,
        });
        return;
    }

    if ELOQUENT_QUERY_METHODS.contains(&php_ref.method.as_str()) {
        // Static call on an unknown class with an Eloquent-style method name
        // is treated as a controller-to-model edge with medium confidence:
        // the target may be a model, but it could be any class exposing the
        // same method name.
        facts.edges.push(FrameworkEdge {
            from_qname: php_ref.enclosing_qname.clone(),
            to_qname: class_name.to_string(),
            kind: FrameworkEdgeKind::ControllerToModel,
            provenance: Provenance::LaravelHeuristic,
            confidence: Confidence::Medium,
        });
    }
}

fn classify_this(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    if !ELOQUENT_RELATIONSHIPS.contains(&php_ref.method.as_str()) {
        return;
    }

    let Some(first) = php_ref.args.first() else {
        facts.diagnostics.push(format!(
            "Eloquent relationship `{}` in `{}` has no related-model argument",
            php_ref.method, php_ref.enclosing_qname
        ));
        return;
    };

    let Some(target) = class_constant_target(first) else {
        facts.diagnostics.push(format!(
            "Eloquent relationship `{}` in `{}` uses a non-literal related-model argument",
            php_ref.method, php_ref.enclosing_qname
        ));
        return;
    };

    facts.edges.push(FrameworkEdge {
        from_qname: php_ref.enclosing_qname.clone(),
        to_qname: target,
        kind: FrameworkEdgeKind::EloquentRelationship,
        provenance: Provenance::LaravelHeuristic,
        confidence: Confidence::High,
    });
}

fn classify_container_binding(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    if !CONTAINER_BINDINGS.contains(&php_ref.method.as_str()) {
        return;
    }

    let abstract_arg = php_ref.args.first();
    let concrete_arg = php_ref.args.get(1);

    let Some(abstract_name) = abstract_arg.and_then(class_constant_target) else {
        facts.diagnostics.push(format!(
            "Service binding `{}` in `{}` has a non-literal abstract argument",
            php_ref.method, php_ref.enclosing_qname
        ));
        return;
    };

    let Some(concrete_name) = concrete_arg.and_then(class_constant_target) else {
        facts.diagnostics.push(format!(
            "Service binding `{}` in `{}` for abstract `{}` has a non-literal concrete argument",
            php_ref.method, php_ref.enclosing_qname, abstract_name
        ));
        return;
    };

    facts.edges.push(FrameworkEdge {
        from_qname: abstract_name,
        to_qname: concrete_name,
        kind: FrameworkEdgeKind::ServiceBinding,
        provenance: Provenance::LaravelHeuristic,
        confidence: Confidence::High,
    });
}

fn classify_function(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    match php_ref.method.as_str() {
        "view" => handle_view_helper(php_ref, facts),
        "event" => handle_event_dispatch(php_ref, facts),
        "dispatch" | "dispatch_now" | "dispatch_sync" => handle_job_dispatch(php_ref, facts),
        _ => {}
    }
}

enum RouteSignature {
    /// `Route::get('path', action)` — path is arg 0, action is arg 1.
    Verb,
    /// `Route::match(verbs, 'path', action)` — verbs is arg 0, path is arg 1,
    /// action is arg 2.
    Match,
}

impl RouteSignature {
    fn path_index(&self) -> usize {
        match self {
            RouteSignature::Verb => 0,
            RouteSignature::Match => 1,
        }
    }

    fn action_index(&self) -> usize {
        match self {
            RouteSignature::Verb => 1,
            RouteSignature::Match => 2,
        }
    }
}

fn handle_route_definition(php_ref: &PhpRef, facts: &mut LaravelFacts, signature: RouteSignature) {
    let required_args = signature.action_index() + 1;
    if php_ref.args.len() < required_args {
        facts.diagnostics.push(format!(
            "Route::{} in `{}` has fewer than {} arguments",
            php_ref.method, php_ref.enclosing_qname, required_args
        ));
        return;
    }

    let from_qname = route_from_qname(php_ref, &signature);
    let action = &php_ref.args[signature.action_index()];

    match action {
        PhpArg::Array(elements) => match (elements.first(), elements.get(1)) {
            (Some(class_arg), Some(PhpArg::String(method))) => {
                let Some(class_name) = class_constant_target(class_arg) else {
                    facts.diagnostics.push(format!(
                        "Route::{} in `{}` uses a non-literal controller class",
                        php_ref.method, php_ref.enclosing_qname
                    ));
                    return;
                };

                facts.edges.push(FrameworkEdge {
                    from_qname,
                    to_qname: format!("{class_name}::{method}"),
                    kind: FrameworkEdgeKind::RouteToController,
                    provenance: Provenance::LaravelHeuristic,
                    confidence: Confidence::High,
                });
            }
            _ => {
                facts.diagnostics.push(format!(
                    "Route::{} in `{}` has an action array that is not [class, method]",
                    php_ref.method, php_ref.enclosing_qname
                ));
            }
        },
        PhpArg::ClassConstant(class_name) => {
            // Invokable single-action controller: `Route::get('path', Foo::class)`.
            facts.edges.push(FrameworkEdge {
                from_qname,
                to_qname: format!("{class_name}::__invoke"),
                kind: FrameworkEdgeKind::RouteToController,
                provenance: Provenance::LaravelHeuristic,
                confidence: Confidence::High,
            });
        }
        PhpArg::String(literal) => match parse_legacy_route_action(literal) {
            Some((class_name, method)) => {
                facts.edges.push(FrameworkEdge {
                    from_qname,
                    to_qname: format!("{class_name}::{method}"),
                    kind: FrameworkEdgeKind::RouteToController,
                    provenance: Provenance::LaravelHeuristic,
                    confidence: Confidence::Medium,
                });
            }
            None => {
                facts.diagnostics.push(format!(
                    "Route::{} in `{}` uses an unrecognised string action `{}`",
                    php_ref.method, php_ref.enclosing_qname, literal
                ));
            }
        },
        _ => {
            facts.diagnostics.push(format!(
                "Route::{} in `{}` uses an unsupported action expression",
                php_ref.method, php_ref.enclosing_qname
            ));
        }
    }
}

fn handle_view_helper(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    let Some(view_name) = php_ref.args.first().and_then(string_literal) else {
        facts.diagnostics.push(format!(
            "view() call in `{}` has a non-literal view name",
            php_ref.enclosing_qname
        ));
        return;
    };

    facts.view_refs.push(ViewRef {
        view_name,
        from_qname: php_ref.enclosing_qname.clone(),
    });
}

fn handle_view_make(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    let Some(view_name) = php_ref.args.first().and_then(string_literal) else {
        facts.diagnostics.push(format!(
            "View::make in `{}` has a non-literal view name",
            php_ref.enclosing_qname
        ));
        return;
    };

    facts.view_refs.push(ViewRef {
        view_name,
        from_qname: php_ref.enclosing_qname.clone(),
    });
}

fn handle_event_dispatch(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    let Some(first) = php_ref.args.first() else {
        facts.diagnostics.push(format!(
            "Event dispatch in `{}` has no event argument",
            php_ref.enclosing_qname
        ));
        return;
    };

    let (target, confidence) = match first {
        PhpArg::NewInstance(name) => (name.clone(), Confidence::High),
        PhpArg::ClassConstant(name) => (name.clone(), Confidence::High),
        // String-keyed events (`event('user.registered', [...])`) name an
        // event channel rather than a class, so attribute with lower
        // confidence to mark the looser link.
        PhpArg::String(name) => (name.clone(), Confidence::Medium),
        _ => {
            facts.diagnostics.push(format!(
                "Event dispatch in `{}` uses an unsupported event expression",
                php_ref.enclosing_qname
            ));
            return;
        }
    };

    facts.edges.push(FrameworkEdge {
        from_qname: php_ref.enclosing_qname.clone(),
        to_qname: target,
        kind: FrameworkEdgeKind::EventListener,
        provenance: Provenance::LaravelHeuristic,
        confidence,
    });
}

fn handle_job_dispatch(php_ref: &PhpRef, facts: &mut LaravelFacts) {
    let Some(first) = php_ref.args.first() else {
        facts.diagnostics.push(format!(
            "Job dispatch in `{}` has no job argument",
            php_ref.enclosing_qname
        ));
        return;
    };

    let target = match first {
        PhpArg::NewInstance(name) => name.clone(),
        PhpArg::ClassConstant(name) => name.clone(),
        _ => {
            facts.diagnostics.push(format!(
                "Job dispatch in `{}` uses an unsupported job expression",
                php_ref.enclosing_qname
            ));
            return;
        }
    };

    facts.edges.push(FrameworkEdge {
        from_qname: php_ref.enclosing_qname.clone(),
        to_qname: target,
        kind: FrameworkEdgeKind::JobDispatch,
        provenance: Provenance::LaravelHeuristic,
        confidence: Confidence::High,
    });
}

fn is_facade_name(name: &str) -> bool {
    FACADE_NAMES.contains(&name)
}

fn class_constant_target(arg: &PhpArg) -> Option<String> {
    match arg {
        PhpArg::ClassConstant(name) => Some(name.clone()),
        _ => None,
    }
}

fn string_literal(arg: &PhpArg) -> Option<String> {
    match arg {
        PhpArg::String(value) => Some(value.clone()),
        _ => None,
    }
}

fn route_from_qname(php_ref: &PhpRef, signature: &RouteSignature) -> String {
    let verb = php_ref.method.as_str();
    if let Some(PhpArg::String(path)) = php_ref.args.get(signature.path_index()) {
        format!("route:{verb} {path}")
    } else {
        format!(
            "route:{verb} {}@{}",
            php_ref.enclosing_qname, php_ref.span.start
        )
    }
}

fn parse_legacy_route_action(literal: &str) -> Option<(String, String)> {
    let (class_part, method_part) = literal.split_once('@')?;
    if class_part.is_empty() || method_part.is_empty() {
        return None;
    }
    Some((class_part.to_string(), method_part.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span { start: 0, end: 0 }
    }

    fn input(refs: Vec<PhpRef>) -> Vec<PhpExtractInput> {
        vec![PhpExtractInput {
            path: PathBuf::from("routes/web.php"),
            nodes: Vec::new(),
            refs,
        }]
    }

    fn only_edge(facts: &LaravelFacts) -> &FrameworkEdge {
        assert_eq!(
            facts.edges.len(),
            1,
            "expected exactly one edge, got {:?}",
            facts.edges
        );
        &facts.edges[0]
    }

    fn assert_heuristic(edge: &FrameworkEdge) {
        assert_eq!(edge.provenance, Provenance::LaravelHeuristic);
    }

    #[test]
    fn route_definition_with_array_action_emits_high_confidence_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("Route".to_string()),
            method: "get".to_string(),
            args: vec![
                PhpArg::String("/users".to_string()),
                PhpArg::Array(vec![
                    PhpArg::ClassConstant("App\\Http\\Controllers\\UserController".to_string()),
                    PhpArg::String("index".to_string()),
                ]),
            ],
            span: span(),
            enclosing_qname: "routes/web.php".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::RouteToController);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.from_qname, "route:get /users");
        assert_eq!(
            edge.to_qname,
            "App\\Http\\Controllers\\UserController::index"
        );
        assert!(facts.diagnostics.is_empty());
    }

    #[test]
    fn route_definition_with_legacy_string_action_emits_medium_confidence_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("Route".to_string()),
            method: "post".to_string(),
            args: vec![
                PhpArg::String("/users".to_string()),
                PhpArg::String("UserController@store".to_string()),
            ],
            span: span(),
            enclosing_qname: "routes/web.php".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::RouteToController);
        assert_eq!(edge.confidence, Confidence::Medium);
        assert_eq!(edge.to_qname, "UserController@store".replace('@', "::"));
    }

    #[test]
    fn route_with_unsupported_action_emits_diagnostic_not_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("Route".to_string()),
            method: "get".to_string(),
            args: vec![PhpArg::String("/users".to_string()), PhpArg::Other],
            span: span(),
            enclosing_qname: "routes/web.php".to_string(),
        }];

        let facts = resolve(&input(refs));
        assert!(facts.edges.is_empty());
        assert_eq!(facts.diagnostics.len(), 1);
        assert!(facts.diagnostics[0].contains("unsupported action"));
    }

    #[test]
    fn controller_method_calling_model_static_emits_controller_to_model_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("App\\Models\\User".to_string()),
            method: "all".to_string(),
            args: Vec::new(),
            span: span(),
            enclosing_qname: "App\\Http\\Controllers\\UserController::index".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::ControllerToModel);
        assert_eq!(edge.confidence, Confidence::Medium);
        assert_eq!(
            edge.from_qname,
            "App\\Http\\Controllers\\UserController::index"
        );
        assert_eq!(edge.to_qname, "App\\Models\\User");
    }

    #[test]
    fn model_relationship_method_emits_eloquent_relationship_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::ThisInstance,
            method: "hasMany".to_string(),
            args: vec![PhpArg::ClassConstant("App\\Models\\Post".to_string())],
            span: span(),
            enclosing_qname: "App\\Models\\User::posts".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::EloquentRelationship);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.from_qname, "App\\Models\\User::posts");
        assert_eq!(edge.to_qname, "App\\Models\\Post");
    }

    #[test]
    fn relationship_without_class_constant_emits_diagnostic() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::ThisInstance,
            method: "hasMany".to_string(),
            args: vec![PhpArg::Other],
            span: span(),
            enclosing_qname: "App\\Models\\User::posts".to_string(),
        }];

        let facts = resolve(&input(refs));
        assert!(facts.edges.is_empty());
        assert_eq!(facts.diagnostics.len(), 1);
        assert!(facts.diagnostics[0].contains("non-literal related-model"));
    }

    #[test]
    fn facade_call_emits_facade_edge_with_facade_name() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("Cache".to_string()),
            method: "get".to_string(),
            args: vec![PhpArg::String("key".to_string())],
            span: span(),
            enclosing_qname: "App\\Services\\Foo::run".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::FacadeCall);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.to_qname, "Cache::get");
    }

    #[test]
    fn event_helper_with_new_instance_emits_event_listener_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::None,
            method: "event".to_string(),
            args: vec![PhpArg::NewInstance(
                "App\\Events\\UserRegistered".to_string(),
            )],
            span: span(),
            enclosing_qname: "App\\Services\\Registration::register".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::EventListener);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.to_qname, "App\\Events\\UserRegistered");
    }

    #[test]
    fn dispatch_helper_with_new_instance_emits_job_dispatch_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::None,
            method: "dispatch".to_string(),
            args: vec![PhpArg::NewInstance("App\\Jobs\\SendEmail".to_string())],
            span: span(),
            enclosing_qname: "App\\Http\\Controllers\\MailController::send".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::JobDispatch);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.to_qname, "App\\Jobs\\SendEmail");
    }

    #[test]
    fn view_helper_emits_view_ref_with_literal_name() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::None,
            method: "view".to_string(),
            args: vec![
                PhpArg::String("users.show".to_string()),
                PhpArg::Array(Vec::new()),
            ],
            span: span(),
            enclosing_qname: "App\\Http\\Controllers\\UserController::show".to_string(),
        }];

        let facts = resolve(&input(refs));
        assert!(facts.edges.is_empty());
        assert_eq!(facts.view_refs.len(), 1);
        let view_ref = &facts.view_refs[0];
        assert_eq!(view_ref.view_name, "users.show");
        assert_eq!(
            view_ref.from_qname,
            "App\\Http\\Controllers\\UserController::show"
        );
    }

    #[test]
    fn view_make_facade_emits_view_ref() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("View".to_string()),
            method: "make".to_string(),
            args: vec![PhpArg::String("admin.dashboard".to_string())],
            span: span(),
            enclosing_qname: "App\\Http\\Controllers\\AdminController::index".to_string(),
        }];

        let facts = resolve(&input(refs));
        assert!(facts.edges.is_empty());
        assert_eq!(facts.view_refs.len(), 1);
        assert_eq!(facts.view_refs[0].view_name, "admin.dashboard");
    }

    #[test]
    fn view_helper_with_non_literal_name_emits_diagnostic() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::None,
            method: "view".to_string(),
            args: vec![PhpArg::Other],
            span: span(),
            enclosing_qname: "App\\Http\\Controllers\\Foo::index".to_string(),
        }];

        let facts = resolve(&input(refs));
        assert!(facts.view_refs.is_empty());
        assert_eq!(facts.diagnostics.len(), 1);
        assert!(facts.diagnostics[0].contains("non-literal view name"));
    }

    #[test]
    fn service_container_binding_emits_service_binding_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::ThisAppContainer,
            method: "bind".to_string(),
            args: vec![
                PhpArg::ClassConstant("App\\Contracts\\Foo".to_string()),
                PhpArg::ClassConstant("App\\Services\\Bar".to_string()),
            ],
            span: span(),
            enclosing_qname: "App\\Providers\\AppServiceProvider::register".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::ServiceBinding);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.from_qname, "App\\Contracts\\Foo");
        assert_eq!(edge.to_qname, "App\\Services\\Bar");
    }

    #[test]
    fn singleton_binding_emits_service_binding_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::ThisAppContainer,
            method: "singleton".to_string(),
            args: vec![
                PhpArg::ClassConstant("App\\Contracts\\Foo".to_string()),
                PhpArg::ClassConstant("App\\Services\\Bar".to_string()),
            ],
            span: span(),
            enclosing_qname: "App\\Providers\\AppServiceProvider::register".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::ServiceBinding);
    }

    #[test]
    fn binding_with_non_literal_arguments_emits_diagnostic() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::ThisAppContainer,
            method: "bind".to_string(),
            args: vec![PhpArg::Other, PhpArg::Other],
            span: span(),
            enclosing_qname: "App\\Providers\\AppServiceProvider::register".to_string(),
        }];

        let facts = resolve(&input(refs));
        assert!(facts.edges.is_empty());
        assert_eq!(facts.diagnostics.len(), 1);
    }

    #[test]
    fn unrelated_receiver_emits_nothing() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::OtherInstance,
            method: "doSomething".to_string(),
            args: Vec::new(),
            span: span(),
            enclosing_qname: "App\\Services\\Foo::run".to_string(),
        }];

        let facts = resolve(&input(refs));
        assert!(facts.edges.is_empty());
        assert!(facts.view_refs.is_empty());
        assert!(facts.diagnostics.is_empty());
    }

    #[test]
    fn route_match_uses_third_argument_as_action() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("Route".to_string()),
            method: "match".to_string(),
            args: vec![
                PhpArg::Array(vec![
                    PhpArg::String("get".to_string()),
                    PhpArg::String("post".to_string()),
                ]),
                PhpArg::String("/users".to_string()),
                PhpArg::Array(vec![
                    PhpArg::ClassConstant("App\\Http\\Controllers\\UserController".to_string()),
                    PhpArg::String("upsert".to_string()),
                ]),
            ],
            span: span(),
            enclosing_qname: "routes/web.php".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::RouteToController);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.from_qname, "route:match /users");
        assert_eq!(
            edge.to_qname,
            "App\\Http\\Controllers\\UserController::upsert"
        );
    }

    #[test]
    fn route_with_invokable_controller_emits_invoke_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("Route".to_string()),
            method: "get".to_string(),
            args: vec![
                PhpArg::String("/dashboard".to_string()),
                PhpArg::ClassConstant("App\\Http\\Controllers\\DashboardController".to_string()),
            ],
            span: span(),
            enclosing_qname: "routes/web.php".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::RouteToController);
        assert_eq!(edge.confidence, Confidence::High);
        assert_eq!(edge.from_qname, "route:get /dashboard");
        assert_eq!(
            edge.to_qname,
            "App\\Http\\Controllers\\DashboardController::__invoke"
        );
    }

    #[test]
    fn route_from_qname_includes_http_verb_to_avoid_collisions() {
        let refs = vec![
            PhpRef {
                receiver: PhpCallReceiver::StaticClass("Route".to_string()),
                method: "get".to_string(),
                args: vec![
                    PhpArg::String("/users".to_string()),
                    PhpArg::Array(vec![
                        PhpArg::ClassConstant("UserController".to_string()),
                        PhpArg::String("index".to_string()),
                    ]),
                ],
                span: span(),
                enclosing_qname: "routes/web.php".to_string(),
            },
            PhpRef {
                receiver: PhpCallReceiver::StaticClass("Route".to_string()),
                method: "post".to_string(),
                args: vec![
                    PhpArg::String("/users".to_string()),
                    PhpArg::Array(vec![
                        PhpArg::ClassConstant("UserController".to_string()),
                        PhpArg::String("store".to_string()),
                    ]),
                ],
                span: span(),
                enclosing_qname: "routes/web.php".to_string(),
            },
        ];

        let facts = resolve(&input(refs));
        assert_eq!(facts.edges.len(), 2);
        assert_eq!(facts.edges[0].from_qname, "route:get /users");
        assert_eq!(facts.edges[1].from_qname, "route:post /users");
        assert_ne!(facts.edges[0].from_qname, facts.edges[1].from_qname);
    }

    #[test]
    fn string_event_name_emits_medium_confidence_event_edge() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::None,
            method: "event".to_string(),
            args: vec![PhpArg::String("user.registered".to_string())],
            span: span(),
            enclosing_qname: "App\\Services\\Foo::run".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::EventListener);
        assert_eq!(edge.confidence, Confidence::Medium);
        assert_eq!(edge.to_qname, "user.registered");
    }

    #[test]
    fn event_facade_method_other_than_dispatch_is_treated_as_facade_call() {
        let refs = vec![PhpRef {
            receiver: PhpCallReceiver::StaticClass("Event".to_string()),
            method: "listen".to_string(),
            args: vec![PhpArg::String("user.registered".to_string())],
            span: span(),
            enclosing_qname: "App\\Providers\\EventServiceProvider::boot".to_string(),
        }];

        let facts = resolve(&input(refs));
        let edge = only_edge(&facts);

        assert_heuristic(edge);
        assert_eq!(edge.kind, FrameworkEdgeKind::FacadeCall);
        assert_eq!(edge.to_qname, "Event::listen");
    }

    #[test]
    fn every_edge_carries_laravel_heuristic_provenance() {
        let refs = vec![
            PhpRef {
                receiver: PhpCallReceiver::StaticClass("Route".to_string()),
                method: "get".to_string(),
                args: vec![
                    PhpArg::String("/a".to_string()),
                    PhpArg::Array(vec![
                        PhpArg::ClassConstant("A".to_string()),
                        PhpArg::String("b".to_string()),
                    ]),
                ],
                span: span(),
                enclosing_qname: "routes/web.php".to_string(),
            },
            PhpRef {
                receiver: PhpCallReceiver::StaticClass("Cache".to_string()),
                method: "get".to_string(),
                args: vec![PhpArg::String("k".to_string())],
                span: span(),
                enclosing_qname: "F::g".to_string(),
            },
            PhpRef {
                receiver: PhpCallReceiver::ThisInstance,
                method: "belongsTo".to_string(),
                args: vec![PhpArg::ClassConstant("Owner".to_string())],
                span: span(),
                enclosing_qname: "M::owner".to_string(),
            },
            PhpRef {
                receiver: PhpCallReceiver::None,
                method: "event".to_string(),
                args: vec![PhpArg::NewInstance("E".to_string())],
                span: span(),
                enclosing_qname: "S::run".to_string(),
            },
            PhpRef {
                receiver: PhpCallReceiver::ThisAppContainer,
                method: "singleton".to_string(),
                args: vec![
                    PhpArg::ClassConstant("Abs".to_string()),
                    PhpArg::ClassConstant("Conc".to_string()),
                ],
                span: span(),
                enclosing_qname: "Provider::register".to_string(),
            },
        ];

        let facts = resolve(&input(refs));
        assert!(facts.edges.len() >= 5);
        for edge in &facts.edges {
            assert_eq!(edge.provenance, Provenance::LaravelHeuristic);
        }
    }

    fn blade_input(path: &str, refs: Vec<BladeRef>) -> Vec<BladeExtractInput> {
        vec![BladeExtractInput {
            path: PathBuf::from(path),
            refs,
        }]
    }

    fn blade_ref(kind: BladeRefKind, value: &str) -> BladeRef {
        BladeRef {
            kind,
            value: value.to_owned(),
            span: span(),
        }
    }

    #[test]
    fn blade_view_qname_from_resources_views() {
        assert_eq!(
            blade_view_qname_from_path(std::path::Path::new(
                "resources/views/users/index.blade.php"
            )),
            Some("view.users.index".to_string())
        );
        assert_eq!(
            blade_view_qname_from_path(std::path::Path::new("resources/views/layout.blade.php")),
            Some("view.layout".to_string())
        );
    }

    #[test]
    fn blade_view_qname_returns_none_outside_views_tree() {
        assert_eq!(
            blade_view_qname_from_path(std::path::Path::new("app/Http/Controllers/X.php")),
            None
        );
        assert_eq!(
            blade_view_qname_from_path(std::path::Path::new("resources/lang/en.php")),
            None
        );
    }

    #[test]
    fn blade_extends_emits_extends_edge() {
        let facts = resolve_blade(&blade_input(
            "resources/views/users/index.blade.php",
            vec![blade_ref(BladeRefKind::ExtendsView, "layouts.app")],
        ));
        let edge = only_edge(&facts);
        assert_heuristic(edge);
        assert_eq!(edge.from_qname, "view.users.index");
        assert_eq!(edge.to_qname, "view.layouts.app");
        assert_eq!(edge.kind, FrameworkEdgeKind::BladeExtendsView);
        assert_eq!(edge.confidence, Confidence::High);
    }

    #[test]
    fn blade_include_emits_include_edge() {
        let facts = resolve_blade(&blade_input(
            "resources/views/users/index.blade.php",
            vec![blade_ref(BladeRefKind::IncludesView, "partials.header")],
        ));
        let edge = only_edge(&facts);
        assert_heuristic(edge);
        assert_eq!(edge.to_qname, "view.partials.header");
        assert_eq!(edge.kind, FrameworkEdgeKind::BladeIncludesView);
    }

    #[test]
    fn blade_component_emits_uses_component_edge() {
        let facts = resolve_blade(&blade_input(
            "resources/views/layout.blade.php",
            vec![
                blade_ref(BladeRefKind::Component, "alert"),
                blade_ref(BladeRefKind::XComponent, "card.body"),
            ],
        ));
        assert_eq!(facts.edges.len(), 2);
        assert!(
            facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::BladeUsesComponent
                    && e.to_qname == "component.alert")
        );
        assert!(
            facts
                .edges
                .iter()
                .any(|e| e.kind == FrameworkEdgeKind::BladeUsesComponent
                    && e.to_qname == "component.card.body")
        );
    }

    #[test]
    fn blade_input_outside_views_tree_emits_no_edges() {
        let facts = resolve_blade(&blade_input(
            "app/templates/foo.blade.php",
            vec![blade_ref(BladeRefKind::ExtendsView, "layouts.app")],
        ));
        assert!(
            facts.edges.is_empty(),
            "blade templates outside resources/views/ must not synthesize view edges"
        );
    }
}
