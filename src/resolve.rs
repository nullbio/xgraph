//! Central node/ref/edge kind enums and deterministic symbol resolution.
//!
//! These types are the vocabulary used across extraction, graph
//! materialization, and the resolver pass. The resolver maps unresolved
//! references against a deterministic [`SymbolTable`]: exact qualified-name
//! matches succeed only when there is a single candidate; ambiguity, missing
//! candidates, and heuristic fallbacks are signalled to the caller so they can
//! be recorded as low-confidence or diagnostic facts rather than invented.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Module,
    Namespace,
    Class,
    Trait,
    Interface,
    Enum,
    Function,
    Method,
    Property,
    Constant,
    Variable,
    ImportTarget,
    ExportTarget,
    Route,
    View,
    Component,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    Call,
    Inherit,
    Implement,
    TraitUse,
    Import,
    Export,
    TypeReference,
    JsxComponent,
    BladeView,
    Decorator,
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Calls,
    Inherits,
    Implements,
    Uses,
    Imports,
    Exports,
    References,
    RoutesTo,
    Renders,
    DispatchesTo,
    ListensFor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provenance {
    ParserExtract,
    LaravelHeuristic,
    JsxComponent,
    BladeReference,
    ImportResolver,
    TypeResolver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
    Exact,
}

/// Local newtype for node identity inside the resolver. This is intentionally
/// independent of `crate::indexes::NodeId` so neither module depends on the
/// other; integration will unify them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

#[derive(Debug, Default, Clone)]
pub struct ResolutionHints {
    pub current_namespace: Option<String>,
    pub imports: Vec<(String, String)>,
}

#[derive(Debug, Default, Clone)]
pub struct SymbolTable {
    by_qname: HashMap<String, Vec<NodeId>>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, qname: String, node_id: NodeId) {
        self.by_qname.entry(qname).or_default().push(node_id);
    }

    pub fn resolve_exact(&self, qname: &str) -> Option<NodeId> {
        match self.by_qname.get(qname) {
            Some(candidates) if candidates.len() == 1 => Some(candidates[0]),
            _ => None,
        }
    }

    pub fn resolve_candidates(&self, qname: &str) -> &[NodeId] {
        match self.by_qname.get(qname) {
            Some(candidates) => candidates.as_slice(),
            None => &[],
        }
    }
}

/// Classify an unresolved reference name against the supplied hints.
///
/// For the first cut this only returns [`RefKind::Unresolved`]; richer
/// classification (import-rewritten qnames, namespace-relative lookups,
/// framework heuristics) plugs in here without changing callers.
pub fn classify_unresolved(_qname: &str, _hints: &ResolutionHints) -> RefKind {
    RefKind::Unresolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_kind_debug_includes_variant_name() {
        assert_eq!(format!("{:?}", NodeKind::Module), "Module");
        assert_eq!(format!("{:?}", NodeKind::Namespace), "Namespace");
        assert_eq!(format!("{:?}", NodeKind::Class), "Class");
        assert_eq!(format!("{:?}", NodeKind::Trait), "Trait");
        assert_eq!(format!("{:?}", NodeKind::Interface), "Interface");
        assert_eq!(format!("{:?}", NodeKind::Enum), "Enum");
        assert_eq!(format!("{:?}", NodeKind::Function), "Function");
        assert_eq!(format!("{:?}", NodeKind::Method), "Method");
        assert_eq!(format!("{:?}", NodeKind::Property), "Property");
        assert_eq!(format!("{:?}", NodeKind::Constant), "Constant");
        assert_eq!(format!("{:?}", NodeKind::Variable), "Variable");
        assert_eq!(format!("{:?}", NodeKind::ImportTarget), "ImportTarget");
        assert_eq!(format!("{:?}", NodeKind::ExportTarget), "ExportTarget");
        assert_eq!(format!("{:?}", NodeKind::Route), "Route");
        assert_eq!(format!("{:?}", NodeKind::View), "View");
        assert_eq!(format!("{:?}", NodeKind::Component), "Component");
        assert_eq!(format!("{:?}", NodeKind::Other), "Other");
    }

    #[test]
    fn ref_kind_debug_includes_variant_name() {
        assert_eq!(format!("{:?}", RefKind::Call), "Call");
        assert_eq!(format!("{:?}", RefKind::Inherit), "Inherit");
        assert_eq!(format!("{:?}", RefKind::Implement), "Implement");
        assert_eq!(format!("{:?}", RefKind::TraitUse), "TraitUse");
        assert_eq!(format!("{:?}", RefKind::Import), "Import");
        assert_eq!(format!("{:?}", RefKind::Export), "Export");
        assert_eq!(format!("{:?}", RefKind::TypeReference), "TypeReference");
        assert_eq!(format!("{:?}", RefKind::JsxComponent), "JsxComponent");
        assert_eq!(format!("{:?}", RefKind::BladeView), "BladeView");
        assert_eq!(format!("{:?}", RefKind::Decorator), "Decorator");
        assert_eq!(format!("{:?}", RefKind::Unresolved), "Unresolved");
    }

    #[test]
    fn edge_kind_debug_includes_variant_name() {
        assert_eq!(format!("{:?}", EdgeKind::Calls), "Calls");
        assert_eq!(format!("{:?}", EdgeKind::Inherits), "Inherits");
        assert_eq!(format!("{:?}", EdgeKind::Implements), "Implements");
        assert_eq!(format!("{:?}", EdgeKind::Uses), "Uses");
        assert_eq!(format!("{:?}", EdgeKind::Imports), "Imports");
        assert_eq!(format!("{:?}", EdgeKind::Exports), "Exports");
        assert_eq!(format!("{:?}", EdgeKind::References), "References");
        assert_eq!(format!("{:?}", EdgeKind::RoutesTo), "RoutesTo");
        assert_eq!(format!("{:?}", EdgeKind::Renders), "Renders");
        assert_eq!(format!("{:?}", EdgeKind::DispatchesTo), "DispatchesTo");
        assert_eq!(format!("{:?}", EdgeKind::ListensFor), "ListensFor");
    }

    #[test]
    fn provenance_debug_includes_variant_name() {
        assert_eq!(format!("{:?}", Provenance::ParserExtract), "ParserExtract");
        assert_eq!(
            format!("{:?}", Provenance::LaravelHeuristic),
            "LaravelHeuristic"
        );
        assert_eq!(format!("{:?}", Provenance::JsxComponent), "JsxComponent");
        assert_eq!(
            format!("{:?}", Provenance::BladeReference),
            "BladeReference"
        );
        assert_eq!(
            format!("{:?}", Provenance::ImportResolver),
            "ImportResolver"
        );
        assert_eq!(format!("{:?}", Provenance::TypeResolver), "TypeResolver");
    }

    #[test]
    fn confidence_debug_includes_variant_name() {
        assert_eq!(format!("{:?}", Confidence::Low), "Low");
        assert_eq!(format!("{:?}", Confidence::Medium), "Medium");
        assert_eq!(format!("{:?}", Confidence::High), "High");
        assert_eq!(format!("{:?}", Confidence::Exact), "Exact");
    }

    #[test]
    fn confidence_ordering_is_low_to_exact() {
        assert!(Confidence::Exact > Confidence::High);
        assert!(Confidence::High > Confidence::Medium);
        assert!(Confidence::Medium > Confidence::Low);
    }

    #[test]
    fn register_then_resolve_exact_returns_registered_id() {
        let mut table = SymbolTable::new();
        table.register("App\\Models\\User".to_string(), NodeId(7));
        assert_eq!(table.resolve_exact("App\\Models\\User"), Some(NodeId(7)));
    }

    #[test]
    fn resolve_exact_returns_none_for_ambiguous_qname() {
        let mut table = SymbolTable::new();
        table.register("App\\Service".to_string(), NodeId(1));
        table.register("App\\Service".to_string(), NodeId(2));
        assert_eq!(table.resolve_exact("App\\Service"), None);
    }

    #[test]
    fn resolve_exact_returns_none_for_unknown_qname() {
        let table = SymbolTable::new();
        assert_eq!(table.resolve_exact("Nope"), None);
    }

    #[test]
    fn resolve_candidates_returns_all_registered_ids() {
        let mut table = SymbolTable::new();
        table.register("App\\Service".to_string(), NodeId(1));
        table.register("App\\Service".to_string(), NodeId(2));
        table.register("App\\Service".to_string(), NodeId(3));
        assert_eq!(
            table.resolve_candidates("App\\Service"),
            &[NodeId(1), NodeId(2), NodeId(3)]
        );
    }

    #[test]
    fn resolve_candidates_returns_empty_slice_for_unknown_qname() {
        let table = SymbolTable::new();
        assert!(table.resolve_candidates("missing").is_empty());
    }

    #[test]
    fn classify_unresolved_returns_unresolved_with_empty_hints() {
        let hints = ResolutionHints::default();
        assert_eq!(
            classify_unresolved("App\\Models\\User", &hints),
            RefKind::Unresolved
        );
    }
}
