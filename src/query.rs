//! Centralized Cozo Datalog queries for complex graph analysis.
//!
//! This module defines reusable query strings as constants alongside typed
//! parameter and result wrappers. It deliberately avoids depending on the
//! `cozo` crate so it can be compiled and tested standalone. A real executor
//! is wired in at integration time by implementing [`CozoQueryExecutor`].
//!
//! The queries assume the schema documented in the project README, in
//! particular the stored relations:
//!
//! - `edge[source_node_id, kind, target_node_id] => provenance, confidence`
//! - `active_node[node_id] => path, content_hash, local_node_id, kind, name, qname, span`

use std::collections::BTreeMap;
use std::fmt;

/// A value passed into or returned from a Cozo query.
///
/// This intentionally mirrors the small subset of Cozo value types xgraph
/// uses, so the query layer never has to import the `cozo` crate.
#[derive(Debug, Clone, PartialEq)]
pub enum CozoValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<CozoValue>),
}

/// Executor that runs a parameterized Cozo query string and returns a row set.
pub trait CozoQueryExecutor {
    type Error;

    fn run(
        &self,
        query: &str,
        params: BTreeMap<String, CozoValue>,
    ) -> Result<Vec<BTreeMap<String, CozoValue>>, Self::Error>;
}

/// Failure modes when translating values between typed Rust inputs/outputs
/// and Cozo query values.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryError {
    /// A row was missing an expected column.
    MissingColumn(&'static str),
    /// A column held a value of an unexpected kind.
    UnexpectedType {
        column: &'static str,
        expected: &'static str,
    },
    /// A numeric column held a value outside the typed Rust range.
    OutOfRange { column: &'static str, value: i64 },
    /// An input argument did not fit in Cozo's `Int` (signed 64-bit) range.
    InputOutOfRange { argument: &'static str, value: u64 },
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::MissingColumn(c) => write!(f, "missing column `{c}` in query result"),
            QueryError::UnexpectedType { column, expected } => {
                write!(
                    f,
                    "column `{column}` had unexpected type, expected {expected}"
                )
            }
            QueryError::OutOfRange { column, value } => {
                write!(f, "column `{column}` value {value} is out of range")
            }
            QueryError::InputOutOfRange { argument, value } => {
                write!(
                    f,
                    "input `{argument}` value {value} exceeds Cozo Int range (max {})",
                    i64::MAX
                )
            }
        }
    }
}

impl std::error::Error for QueryError {}

/// Combined error returned by typed query wrappers: either the executor
/// failed, or the result rows could not be parsed into the typed output.
#[derive(Debug)]
pub enum RunError<E> {
    Executor(E),
    Parse(QueryError),
}

impl<E: fmt::Display> fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Executor(e) => write!(f, "cozo executor error: {e}"),
            RunError::Parse(e) => write!(f, "query result parse error: {e}"),
        }
    }
}

impl<E: fmt::Display + fmt::Debug> std::error::Error for RunError<E> {}

impl<E> From<QueryError> for RunError<E> {
    fn from(value: QueryError) -> Self {
        RunError::Parse(value)
    }
}

/// Edge kinds used by the queries below. Mirrors the values stored in the
/// `edge.kind` column.
pub mod edge_kind {
    pub const CALLS: &str = "Calls";
    pub const INHERITS: &str = "Inherits";
    pub const IMPLEMENTS: &str = "Implements";
    pub const REFERENCES: &str = "References";
}

// ---------------------------------------------------------------------------
// QUERY_TRANSITIVE_IMPACT
// ---------------------------------------------------------------------------

/// Bounded transitive impact: every node reachable from `$start` over any
/// edge kind in at most `$max_depth` hops. Results are deduplicated and
/// sorted for determinism.
pub const QUERY_TRANSITIVE_IMPACT: &str = "\
reachable[node, depth] := *edge[$start, _, node], depth = 1
reachable[node, depth] := reachable[prev, prev_depth], prev_depth < $max_depth, *edge[prev, _, node], depth = prev_depth + 1
?[node] := reachable[node, _]
:sort node
";

/// Unbounded transitive impact: every node reachable from `$start` over any
/// edge kind, with no depth limit. Results are deduplicated and sorted.
pub const QUERY_TRANSITIVE_IMPACT_UNBOUNDED: &str = "\
reachable[node] := *edge[$start, _, node]
reachable[node] := reachable[prev], *edge[prev, _, node]
?[node] := reachable[node]
:sort node
";

/// Typed wrapper for [`QUERY_TRANSITIVE_IMPACT`] and
/// [`QUERY_TRANSITIVE_IMPACT_UNBOUNDED`].
pub struct TransitiveImpact<'a, E> {
    executor: &'a E,
}

impl<'a, E> TransitiveImpact<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }
}

impl<E: CozoQueryExecutor> TransitiveImpact<'_, E> {
    /// `max_depth` of `Some(0)` means "zero hops allowed" and returns an
    /// empty result without invoking the executor. `Some(n)` for n > 0
    /// follows up to n edges. `None` is unbounded.
    pub fn run(
        &self,
        node_id: u64,
        max_depth: Option<u32>,
    ) -> Result<Vec<u64>, RunError<E::Error>> {
        if matches!(max_depth, Some(0)) {
            return Ok(Vec::new());
        }

        let mut params = BTreeMap::new();
        params.insert("start".to_string(), node_id_value("node_id", node_id)?);

        let query = match max_depth {
            Some(depth) => {
                params.insert("max_depth".to_string(), CozoValue::Int(i64::from(depth)));
                QUERY_TRANSITIVE_IMPACT
            }
            None => QUERY_TRANSITIVE_IMPACT_UNBOUNDED,
        };

        let rows = self
            .executor
            .run(query, params)
            .map_err(RunError::Executor)?;
        parse_node_id_column(&rows, "node").map_err(RunError::Parse)
    }
}

// ---------------------------------------------------------------------------
// QUERY_CYCLES
// ---------------------------------------------------------------------------

/// Find cycles in the `Calls` edge subgraph using strongly connected
/// components. Any SCC with more than one node, or a self-loop, forms a
/// cycle. Returns one row per cycle node with the component identifier so
/// callers can group nodes by component. Requires Cozo to be built with
/// the `graph-algo` feature.
pub const QUERY_CYCLES: &str = "\
calls_edges[from, to] := *edge[from, kind, to], kind = 'Calls'
scc[node, component] <~ StronglyConnectedComponent(calls_edges[])
self_loop[node] := calls_edges[node, node]
component_size[component, count(node)] := scc[node, component]
?[component, node] := scc[node, component], component_size[component, size], size > 1
?[component, node] := scc[node, component], self_loop[node]
:sort component, node
";

/// One cycle found in the `Calls` subgraph: a component identifier together
/// with the node ids that participate in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cycle {
    pub component: i64,
    pub nodes: Vec<u64>,
}

pub struct Cycles<'a, E> {
    executor: &'a E,
}

impl<'a, E> Cycles<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }
}

impl<E: CozoQueryExecutor> Cycles<'_, E> {
    pub fn run(&self) -> Result<Vec<Cycle>, RunError<E::Error>> {
        let rows = self
            .executor
            .run(QUERY_CYCLES, BTreeMap::new())
            .map_err(RunError::Executor)?;

        let mut grouped: BTreeMap<i64, Vec<u64>> = BTreeMap::new();
        for row in &rows {
            let component = take_int(row, "component")?;
            let node = take_node_id(row, "node")?;
            grouped.entry(component).or_default().push(node);
        }

        Ok(grouped
            .into_iter()
            .map(|(component, nodes)| Cycle { component, nodes })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// QUERY_DEPENDENCY_CONE
// ---------------------------------------------------------------------------

/// All nodes that transitively depend on `$target`: callers via `Calls`,
/// renderers via `Renders`, subclasses via `Inherits`, implementers via
/// `Implements`. Edges are followed *backwards* (caller -> callee becomes
/// callee -> caller in the reverse traversal). Sorted for determinism.
pub const QUERY_DEPENDENCY_CONE: &str = "\
dependency_edge[from, to] := *edge[from, kind, to], kind = 'Calls'
dependency_edge[from, to] := *edge[from, kind, to], kind = 'Renders'
dependency_edge[from, to] := *edge[from, kind, to], kind = 'Inherits'
dependency_edge[from, to] := *edge[from, kind, to], kind = 'Implements'
dependent[node] := dependency_edge[node, $target]
dependent[node] := dependent[downstream], dependency_edge[node, downstream]
?[node] := dependent[node]
:sort node
";

pub struct DependencyCone<'a, E> {
    executor: &'a E,
}

impl<'a, E> DependencyCone<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }
}

impl<E: CozoQueryExecutor> DependencyCone<'_, E> {
    pub fn run(&self, node_id: u64) -> Result<Vec<u64>, RunError<E::Error>> {
        let mut params = BTreeMap::new();
        params.insert("target".to_string(), node_id_value("node_id", node_id)?);

        let rows = self
            .executor
            .run(QUERY_DEPENDENCY_CONE, params)
            .map_err(RunError::Executor)?;
        parse_node_id_column(&rows, "node").map_err(RunError::Parse)
    }
}

// ---------------------------------------------------------------------------
// QUERY_PATH_BETWEEN
// ---------------------------------------------------------------------------

/// Shortest path of edges between `$from` and `$to` over all edge kinds.
/// Edges are treated as unweighted, so `cost` is the hop count. Requires
/// Cozo to be built with the `graph-algo` feature.
pub const QUERY_PATH_BETWEEN: &str = "\
graph_edge[from, to] := *edge[from, _, to]
starting[node] := node = $from
goal[node] := node = $to
?[from_node, goal_node, cost, path] <~ ShortestPathDijkstra(graph_edge[], starting[], goal[])
:sort cost
";

#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub from: u64,
    pub to: u64,
    pub cost: f64,
    pub nodes: Vec<u64>,
}

pub struct PathBetween<'a, E> {
    executor: &'a E,
}

impl<'a, E> PathBetween<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }
}

impl<E: CozoQueryExecutor> PathBetween<'_, E> {
    pub fn run(&self, from: u64, to: u64) -> Result<Option<Path>, RunError<E::Error>> {
        let mut params = BTreeMap::new();
        params.insert("from".to_string(), node_id_value("from", from)?);
        params.insert("to".to_string(), node_id_value("to", to)?);

        let rows = self
            .executor
            .run(QUERY_PATH_BETWEEN, params)
            .map_err(RunError::Executor)?;

        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };

        let from_node = take_node_id(&row, "from_node")?;
        let goal_node = take_node_id(&row, "goal_node")?;
        let cost = take_float(&row, "cost")?;
        let path_value = row.get("path").ok_or(QueryError::MissingColumn("path"))?;
        let CozoValue::List(items) = path_value else {
            return Err(RunError::Parse(QueryError::UnexpectedType {
                column: "path",
                expected: "list",
            }));
        };
        let mut nodes = Vec::with_capacity(items.len());
        for item in items {
            nodes.push(node_id_from_value(item, "path")?);
        }

        Ok(Some(Path {
            from: from_node,
            to: goal_node,
            cost,
            nodes,
        }))
    }
}

// ---------------------------------------------------------------------------
// QUERY_MODULE_BOUNDARY
// ---------------------------------------------------------------------------

/// All edges whose source and target nodes live in different files,
/// optionally scoped by substring patterns on the source and target paths.
///
/// The query always runs the substring filter `str_includes(path, pattern)`.
/// The empty string is the identity element of substring matching — every
/// string contains the empty string — so callers that want no scoping pass
/// `""` for the corresponding pattern. Sorted by source path, target path,
/// then edge kind for determinism.
pub const QUERY_MODULE_BOUNDARY: &str = "\
?[source_node, source_path, kind, target_node, target_path] :=
    *edge[source_node, kind, target_node],
    *active_node{node_id: source_node, path: source_path},
    *active_node{node_id: target_node, path: target_path},
    source_path != target_path,
    str_includes(source_path, $source_pattern),
    str_includes(target_path, $target_pattern)
:sort source_path, target_path, kind
";

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleBoundaryEdge {
    pub source_node: u64,
    pub source_path: String,
    pub kind: String,
    pub target_node: u64,
    pub target_path: String,
}

pub struct ModuleBoundary<'a, E> {
    executor: &'a E,
}

impl<'a, E> ModuleBoundary<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }
}

impl<E: CozoQueryExecutor> ModuleBoundary<'_, E> {
    /// `source_pattern` and `target_pattern` are substring filters on the
    /// source and target file paths. `None` means "no scoping" and is
    /// passed to Cozo as the empty string, which `str_includes` accepts as
    /// matching every path.
    pub fn run(
        &self,
        source_pattern: Option<&str>,
        target_pattern: Option<&str>,
    ) -> Result<Vec<ModuleBoundaryEdge>, RunError<E::Error>> {
        let mut params = BTreeMap::new();
        params.insert(
            "source_pattern".to_string(),
            CozoValue::Str(source_pattern.unwrap_or("").to_string()),
        );
        params.insert(
            "target_pattern".to_string(),
            CozoValue::Str(target_pattern.unwrap_or("").to_string()),
        );

        let rows = self
            .executor
            .run(QUERY_MODULE_BOUNDARY, params)
            .map_err(RunError::Executor)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(ModuleBoundaryEdge {
                source_node: take_node_id(row, "source_node")?,
                source_path: take_string(row, "source_path")?,
                kind: take_string(row, "kind")?,
                target_node: take_node_id(row, "target_node")?,
                target_path: take_string(row, "target_path")?,
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// QUERY_CHANGES_IF
// ---------------------------------------------------------------------------

/// "What changes if `$target` changes": every symbol whose meaning may be
/// affected if the target node's behavior changes. Combines callers,
/// renderers, subclasses, implementers, and references, transitively in the
/// reverse direction. Sorted for determinism.
pub const QUERY_CHANGES_IF: &str = "\
impact_edge[from, to] := *edge[from, kind, to], kind = 'Calls'
impact_edge[from, to] := *edge[from, kind, to], kind = 'Renders'
impact_edge[from, to] := *edge[from, kind, to], kind = 'Inherits'
impact_edge[from, to] := *edge[from, kind, to], kind = 'Implements'
impact_edge[from, to] := *edge[from, kind, to], kind = 'References'
affected[node] := impact_edge[node, $target]
affected[node] := affected[downstream], impact_edge[node, downstream]
?[node] := affected[node]
:sort node
";

pub struct ChangesIf<'a, E> {
    executor: &'a E,
}

impl<'a, E> ChangesIf<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }
}

impl<E: CozoQueryExecutor> ChangesIf<'_, E> {
    pub fn run(&self, node_id: u64) -> Result<Vec<u64>, RunError<E::Error>> {
        let mut params = BTreeMap::new();
        params.insert("target".to_string(), node_id_value("node_id", node_id)?);

        let rows = self
            .executor
            .run(QUERY_CHANGES_IF, params)
            .map_err(RunError::Executor)?;
        parse_node_id_column(&rows, "node").map_err(RunError::Parse)
    }
}

// ---------------------------------------------------------------------------
// Row helpers
// ---------------------------------------------------------------------------

fn node_id_value(argument: &'static str, node_id: u64) -> Result<CozoValue, QueryError> {
    // Node ids are represented as Cozo Ints (signed 64-bit). Reject ids that
    // do not fit rather than silently clamping.
    if node_id > i64::MAX as u64 {
        Err(QueryError::InputOutOfRange {
            argument,
            value: node_id,
        })
    } else {
        Ok(CozoValue::Int(node_id as i64))
    }
}

fn node_id_from_value(value: &CozoValue, column: &'static str) -> Result<u64, QueryError> {
    match value {
        CozoValue::Int(i) => {
            if *i < 0 {
                Err(QueryError::OutOfRange { column, value: *i })
            } else {
                Ok(*i as u64)
            }
        }
        _ => Err(QueryError::UnexpectedType {
            column,
            expected: "int",
        }),
    }
}

fn take_node_id(
    row: &BTreeMap<String, CozoValue>,
    column: &'static str,
) -> Result<u64, QueryError> {
    let value = row.get(column).ok_or(QueryError::MissingColumn(column))?;
    node_id_from_value(value, column)
}

fn take_int(row: &BTreeMap<String, CozoValue>, column: &'static str) -> Result<i64, QueryError> {
    match row.get(column).ok_or(QueryError::MissingColumn(column))? {
        CozoValue::Int(i) => Ok(*i),
        _ => Err(QueryError::UnexpectedType {
            column,
            expected: "int",
        }),
    }
}

fn take_float(row: &BTreeMap<String, CozoValue>, column: &'static str) -> Result<f64, QueryError> {
    match row.get(column).ok_or(QueryError::MissingColumn(column))? {
        CozoValue::Float(f) => Ok(*f),
        CozoValue::Int(i) => Ok(*i as f64),
        _ => Err(QueryError::UnexpectedType {
            column,
            expected: "float",
        }),
    }
}

fn take_string(
    row: &BTreeMap<String, CozoValue>,
    column: &'static str,
) -> Result<String, QueryError> {
    match row.get(column).ok_or(QueryError::MissingColumn(column))? {
        CozoValue::Str(s) => Ok(s.clone()),
        _ => Err(QueryError::UnexpectedType {
            column,
            expected: "string",
        }),
    }
}

fn parse_node_id_column(
    rows: &[BTreeMap<String, CozoValue>],
    column: &'static str,
) -> Result<Vec<u64>, QueryError> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(take_node_id(row, column)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // -----------------------------------------------------------------
    // Recording executor used by all wrapper tests.
    // -----------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq)]
    struct RecordedCall {
        query: String,
        params: BTreeMap<String, CozoValue>,
    }

    struct RecordingExecutor {
        calls: RefCell<Vec<RecordedCall>>,
        result: Vec<BTreeMap<String, CozoValue>>,
    }

    impl RecordingExecutor {
        fn new(result: Vec<BTreeMap<String, CozoValue>>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                result,
            }
        }

        fn take_calls(&self) -> Vec<RecordedCall> {
            self.calls.borrow_mut().drain(..).collect()
        }
    }

    impl CozoQueryExecutor for RecordingExecutor {
        type Error = String;

        fn run(
            &self,
            query: &str,
            params: BTreeMap<String, CozoValue>,
        ) -> Result<Vec<BTreeMap<String, CozoValue>>, Self::Error> {
            self.calls.borrow_mut().push(RecordedCall {
                query: query.to_string(),
                params,
            });
            Ok(self.result.clone())
        }
    }

    struct FailingExecutor;

    impl CozoQueryExecutor for FailingExecutor {
        type Error = String;

        fn run(
            &self,
            _query: &str,
            _params: BTreeMap<String, CozoValue>,
        ) -> Result<Vec<BTreeMap<String, CozoValue>>, Self::Error> {
            Err("boom".to_string())
        }
    }

    fn row(items: &[(&str, CozoValue)]) -> BTreeMap<String, CozoValue> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    // -----------------------------------------------------------------
    // Constant smoke tests: each query is non-empty and contains the
    // expected Cozo Datalog markers.
    // -----------------------------------------------------------------

    fn assert_is_datalog(query: &str) {
        assert!(!query.is_empty(), "query is empty");
        assert!(query.contains("?["), "missing rule head `?[`");
        assert!(query.contains(":="), "missing `:=` rule body");
    }

    #[test]
    fn transitive_impact_query_is_datalog() {
        assert_is_datalog(QUERY_TRANSITIVE_IMPACT);
        assert!(QUERY_TRANSITIVE_IMPACT.contains("$start"));
        assert!(QUERY_TRANSITIVE_IMPACT.contains("$max_depth"));
        assert!(QUERY_TRANSITIVE_IMPACT.contains(":sort"));
        assert!(QUERY_TRANSITIVE_IMPACT.contains("*edge["));
    }

    #[test]
    fn transitive_impact_unbounded_query_is_datalog() {
        assert_is_datalog(QUERY_TRANSITIVE_IMPACT_UNBOUNDED);
        assert!(QUERY_TRANSITIVE_IMPACT_UNBOUNDED.contains("$start"));
        assert!(!QUERY_TRANSITIVE_IMPACT_UNBOUNDED.contains("$max_depth"));
        assert!(QUERY_TRANSITIVE_IMPACT_UNBOUNDED.contains(":sort"));
    }

    #[test]
    fn cycles_query_is_datalog() {
        assert_is_datalog(QUERY_CYCLES);
        assert!(QUERY_CYCLES.contains("StronglyConnectedComponent"));
        assert!(QUERY_CYCLES.contains("'Calls'"));
        assert!(QUERY_CYCLES.contains(":sort"));
    }

    #[test]
    fn dependency_cone_query_is_datalog() {
        assert_is_datalog(QUERY_DEPENDENCY_CONE);
        assert!(QUERY_DEPENDENCY_CONE.contains("$target"));
        assert!(QUERY_DEPENDENCY_CONE.contains("'Calls'"));
        assert!(QUERY_DEPENDENCY_CONE.contains("'Inherits'"));
        assert!(QUERY_DEPENDENCY_CONE.contains("'Implements'"));
        assert!(QUERY_DEPENDENCY_CONE.contains(":sort"));
    }

    #[test]
    fn path_between_query_is_datalog() {
        assert_is_datalog(QUERY_PATH_BETWEEN);
        assert!(QUERY_PATH_BETWEEN.contains("$from"));
        assert!(QUERY_PATH_BETWEEN.contains("$to"));
        assert!(QUERY_PATH_BETWEEN.contains("ShortestPathDijkstra"));
        assert!(QUERY_PATH_BETWEEN.contains(":sort"));
    }

    #[test]
    fn module_boundary_query_is_datalog() {
        assert_is_datalog(QUERY_MODULE_BOUNDARY);
        assert!(QUERY_MODULE_BOUNDARY.contains("$source_pattern"));
        assert!(QUERY_MODULE_BOUNDARY.contains("$target_pattern"));
        assert!(QUERY_MODULE_BOUNDARY.contains("source_path != target_path"));
        assert!(QUERY_MODULE_BOUNDARY.contains("active_node"));
        assert!(QUERY_MODULE_BOUNDARY.contains(":sort"));
    }

    #[test]
    fn changes_if_query_is_datalog() {
        assert_is_datalog(QUERY_CHANGES_IF);
        assert!(QUERY_CHANGES_IF.contains("$target"));
        assert!(QUERY_CHANGES_IF.contains("'Calls'"));
        assert!(QUERY_CHANGES_IF.contains("'Inherits'"));
        assert!(QUERY_CHANGES_IF.contains("'Implements'"));
        assert!(QUERY_CHANGES_IF.contains("'References'"));
        assert!(QUERY_CHANGES_IF.contains(":sort"));
    }

    // -----------------------------------------------------------------
    // Edge kind constants
    // -----------------------------------------------------------------

    #[test]
    fn edge_kind_constants_match_query_literals() {
        assert!(QUERY_CYCLES.contains(edge_kind::CALLS));
        assert!(QUERY_DEPENDENCY_CONE.contains(edge_kind::CALLS));
        assert!(QUERY_DEPENDENCY_CONE.contains(edge_kind::INHERITS));
        assert!(QUERY_DEPENDENCY_CONE.contains(edge_kind::IMPLEMENTS));
        assert!(QUERY_CHANGES_IF.contains(edge_kind::CALLS));
        assert!(QUERY_CHANGES_IF.contains(edge_kind::INHERITS));
        assert!(QUERY_CHANGES_IF.contains(edge_kind::IMPLEMENTS));
        assert!(QUERY_CHANGES_IF.contains(edge_kind::REFERENCES));
    }

    // -----------------------------------------------------------------
    // Golden tests: pin the exact query string. Any accidental edit will
    // be caught here.
    // -----------------------------------------------------------------

    #[test]
    fn golden_transitive_impact() {
        let expected = "reachable[node, depth] := *edge[$start, _, node], depth = 1\nreachable[node, depth] := reachable[prev, prev_depth], prev_depth < $max_depth, *edge[prev, _, node], depth = prev_depth + 1\n?[node] := reachable[node, _]\n:sort node\n";
        assert_eq!(QUERY_TRANSITIVE_IMPACT, expected);
    }

    #[test]
    fn golden_transitive_impact_unbounded() {
        let expected = "reachable[node] := *edge[$start, _, node]\nreachable[node] := reachable[prev], *edge[prev, _, node]\n?[node] := reachable[node]\n:sort node\n";
        assert_eq!(QUERY_TRANSITIVE_IMPACT_UNBOUNDED, expected);
    }

    #[test]
    fn golden_cycles() {
        let expected = "calls_edges[from, to] := *edge[from, kind, to], kind = 'Calls'\nscc[node, component] <~ StronglyConnectedComponent(calls_edges[])\nself_loop[node] := calls_edges[node, node]\ncomponent_size[component, count(node)] := scc[node, component]\n?[component, node] := scc[node, component], component_size[component, size], size > 1\n?[component, node] := scc[node, component], self_loop[node]\n:sort component, node\n";
        assert_eq!(QUERY_CYCLES, expected);
    }

    #[test]
    fn golden_dependency_cone() {
        let expected = "dependency_edge[from, to] := *edge[from, kind, to], kind = 'Calls'\ndependency_edge[from, to] := *edge[from, kind, to], kind = 'Renders'\ndependency_edge[from, to] := *edge[from, kind, to], kind = 'Inherits'\ndependency_edge[from, to] := *edge[from, kind, to], kind = 'Implements'\ndependent[node] := dependency_edge[node, $target]\ndependent[node] := dependent[downstream], dependency_edge[node, downstream]\n?[node] := dependent[node]\n:sort node\n";
        assert_eq!(QUERY_DEPENDENCY_CONE, expected);
    }

    #[test]
    fn golden_path_between() {
        let expected = "graph_edge[from, to] := *edge[from, _, to]\nstarting[node] := node = $from\ngoal[node] := node = $to\n?[from_node, goal_node, cost, path] <~ ShortestPathDijkstra(graph_edge[], starting[], goal[])\n:sort cost\n";
        assert_eq!(QUERY_PATH_BETWEEN, expected);
    }

    #[test]
    fn golden_module_boundary() {
        let expected = "?[source_node, source_path, kind, target_node, target_path] :=\n    *edge[source_node, kind, target_node],\n    *active_node{node_id: source_node, path: source_path},\n    *active_node{node_id: target_node, path: target_path},\n    source_path != target_path,\n    str_includes(source_path, $source_pattern),\n    str_includes(target_path, $target_pattern)\n:sort source_path, target_path, kind\n";
        assert_eq!(QUERY_MODULE_BOUNDARY, expected);
    }

    #[test]
    fn golden_changes_if() {
        let expected = "impact_edge[from, to] := *edge[from, kind, to], kind = 'Calls'\nimpact_edge[from, to] := *edge[from, kind, to], kind = 'Renders'\nimpact_edge[from, to] := *edge[from, kind, to], kind = 'Inherits'\nimpact_edge[from, to] := *edge[from, kind, to], kind = 'Implements'\nimpact_edge[from, to] := *edge[from, kind, to], kind = 'References'\naffected[node] := impact_edge[node, $target]\naffected[node] := affected[downstream], impact_edge[node, downstream]\n?[node] := affected[node]\n:sort node\n";
        assert_eq!(QUERY_CHANGES_IF, expected);
    }

    // -----------------------------------------------------------------
    // Wrapper tests
    // -----------------------------------------------------------------

    #[test]
    fn transitive_impact_bounded_sends_depth_param() {
        let executor = RecordingExecutor::new(vec![
            row(&[("node", CozoValue::Int(2))]),
            row(&[("node", CozoValue::Int(7))]),
        ]);
        let result = TransitiveImpact::new(&executor)
            .run(42, Some(5))
            .expect("query should succeed");
        assert_eq!(result, vec![2, 7]);

        let calls = executor.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, QUERY_TRANSITIVE_IMPACT);
        let mut expected = BTreeMap::new();
        expected.insert("start".to_string(), CozoValue::Int(42));
        expected.insert("max_depth".to_string(), CozoValue::Int(5));
        assert_eq!(calls[0].params, expected);
    }

    #[test]
    fn transitive_impact_unbounded_uses_unbounded_query() {
        let executor = RecordingExecutor::new(vec![row(&[("node", CozoValue::Int(11))])]);
        let result = TransitiveImpact::new(&executor)
            .run(1, None)
            .expect("query should succeed");
        assert_eq!(result, vec![11]);

        let calls = executor.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, QUERY_TRANSITIVE_IMPACT_UNBOUNDED);
        let mut expected = BTreeMap::new();
        expected.insert("start".to_string(), CozoValue::Int(1));
        assert_eq!(calls[0].params, expected);
    }

    #[test]
    fn transitive_impact_propagates_executor_error() {
        let result = TransitiveImpact::new(&FailingExecutor).run(1, None);
        match result {
            Err(RunError::Executor(e)) => assert_eq!(e, "boom"),
            other => panic!("expected executor error, got {other:?}"),
        }
    }

    #[test]
    fn transitive_impact_reports_parse_error() {
        let executor =
            RecordingExecutor::new(vec![row(&[("node", CozoValue::Str("not-int".into()))])]);
        let err = TransitiveImpact::new(&executor)
            .run(1, None)
            .expect_err("expected parse error");
        match err {
            RunError::Parse(QueryError::UnexpectedType { column, expected }) => {
                assert_eq!(column, "node");
                assert_eq!(expected, "int");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn cycles_groups_nodes_by_component() {
        let executor = RecordingExecutor::new(vec![
            row(&[
                ("component", CozoValue::Int(0)),
                ("node", CozoValue::Int(1)),
            ]),
            row(&[
                ("component", CozoValue::Int(0)),
                ("node", CozoValue::Int(2)),
            ]),
            row(&[
                ("component", CozoValue::Int(3)),
                ("node", CozoValue::Int(9)),
            ]),
        ]);

        let cycles = Cycles::new(&executor).run().expect("query should succeed");
        assert_eq!(
            cycles,
            vec![
                Cycle {
                    component: 0,
                    nodes: vec![1, 2],
                },
                Cycle {
                    component: 3,
                    nodes: vec![9],
                },
            ]
        );

        let calls = executor.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, QUERY_CYCLES);
        assert!(calls[0].params.is_empty());
    }

    #[test]
    fn dependency_cone_sends_target_and_parses_nodes() {
        let executor = RecordingExecutor::new(vec![
            row(&[("node", CozoValue::Int(5))]),
            row(&[("node", CozoValue::Int(8))]),
        ]);
        let result = DependencyCone::new(&executor)
            .run(100)
            .expect("query should succeed");
        assert_eq!(result, vec![5, 8]);

        let calls = executor.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, QUERY_DEPENDENCY_CONE);
        let mut expected = BTreeMap::new();
        expected.insert("target".to_string(), CozoValue::Int(100));
        assert_eq!(calls[0].params, expected);
    }

    #[test]
    fn path_between_parses_path_list() {
        let executor = RecordingExecutor::new(vec![row(&[
            ("from_node", CozoValue::Int(1)),
            ("goal_node", CozoValue::Int(3)),
            ("cost", CozoValue::Float(2.0)),
            (
                "path",
                CozoValue::List(vec![
                    CozoValue::Int(1),
                    CozoValue::Int(2),
                    CozoValue::Int(3),
                ]),
            ),
        ])]);
        let result = PathBetween::new(&executor)
            .run(1, 3)
            .expect("query should succeed");
        assert_eq!(
            result,
            Some(Path {
                from: 1,
                to: 3,
                cost: 2.0,
                nodes: vec![1, 2, 3],
            })
        );

        let calls = executor.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, QUERY_PATH_BETWEEN);
        let mut expected = BTreeMap::new();
        expected.insert("from".to_string(), CozoValue::Int(1));
        expected.insert("to".to_string(), CozoValue::Int(3));
        assert_eq!(calls[0].params, expected);
    }

    #[test]
    fn path_between_returns_none_when_no_rows() {
        let executor = RecordingExecutor::new(vec![]);
        let result = PathBetween::new(&executor)
            .run(1, 2)
            .expect("query should succeed");
        assert_eq!(result, None);
    }

    #[test]
    fn path_between_accepts_integer_cost() {
        let executor = RecordingExecutor::new(vec![row(&[
            ("from_node", CozoValue::Int(1)),
            ("goal_node", CozoValue::Int(2)),
            ("cost", CozoValue::Int(3)),
            (
                "path",
                CozoValue::List(vec![CozoValue::Int(1), CozoValue::Int(2)]),
            ),
        ])]);
        let result = PathBetween::new(&executor)
            .run(1, 2)
            .expect("query should succeed");
        assert_eq!(result.expect("path present").cost, 3.0);
    }

    #[test]
    fn module_boundary_sends_patterns_and_parses_rows() {
        let executor = RecordingExecutor::new(vec![row(&[
            ("source_node", CozoValue::Int(1)),
            ("source_path", CozoValue::Str("a.rs".into())),
            ("kind", CozoValue::Str("Calls".into())),
            ("target_node", CozoValue::Int(2)),
            ("target_path", CozoValue::Str("b.rs".into())),
        ])]);

        let result = ModuleBoundary::new(&executor)
            .run(Some("src/"), Some("tests/"))
            .expect("query should succeed");
        assert_eq!(
            result,
            vec![ModuleBoundaryEdge {
                source_node: 1,
                source_path: "a.rs".to_string(),
                kind: "Calls".to_string(),
                target_node: 2,
                target_path: "b.rs".to_string(),
            }]
        );

        let calls = executor.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, QUERY_MODULE_BOUNDARY);
        let mut expected = BTreeMap::new();
        expected.insert("source_pattern".to_string(), CozoValue::Str("src/".into()));
        expected.insert(
            "target_pattern".to_string(),
            CozoValue::Str("tests/".into()),
        );
        assert_eq!(calls[0].params, expected);
    }

    #[test]
    fn module_boundary_none_patterns_send_empty_string() {
        let executor = RecordingExecutor::new(vec![]);
        let _ = ModuleBoundary::new(&executor)
            .run(None, None)
            .expect("query should succeed");

        let calls = executor.take_calls();
        let mut expected = BTreeMap::new();
        expected.insert("source_pattern".to_string(), CozoValue::Str("".into()));
        expected.insert("target_pattern".to_string(), CozoValue::Str("".into()));
        assert_eq!(calls[0].params, expected);
    }

    #[test]
    fn module_boundary_mixed_pattern_options() {
        let executor = RecordingExecutor::new(vec![]);
        let _ = ModuleBoundary::new(&executor)
            .run(Some("src/"), None)
            .expect("query should succeed");

        let calls = executor.take_calls();
        let mut expected = BTreeMap::new();
        expected.insert("source_pattern".to_string(), CozoValue::Str("src/".into()));
        expected.insert("target_pattern".to_string(), CozoValue::Str("".into()));
        assert_eq!(calls[0].params, expected);
    }

    #[test]
    fn changes_if_sends_target_and_parses_nodes() {
        let executor = RecordingExecutor::new(vec![
            row(&[("node", CozoValue::Int(10))]),
            row(&[("node", CozoValue::Int(20))]),
        ]);
        let result = ChangesIf::new(&executor)
            .run(5)
            .expect("query should succeed");
        assert_eq!(result, vec![10, 20]);

        let calls = executor.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, QUERY_CHANGES_IF);
        let mut expected = BTreeMap::new();
        expected.insert("target".to_string(), CozoValue::Int(5));
        assert_eq!(calls[0].params, expected);
    }

    // -----------------------------------------------------------------
    // Value helpers
    // -----------------------------------------------------------------

    #[test]
    fn node_id_value_accepts_in_range_values() {
        assert_eq!(
            node_id_value("x", 0).expect("0 fits in i64"),
            CozoValue::Int(0)
        );
        assert_eq!(
            node_id_value("x", i64::MAX as u64).expect("i64::MAX fits"),
            CozoValue::Int(i64::MAX)
        );
    }

    #[test]
    fn node_id_value_rejects_above_i64_max() {
        let err =
            node_id_value("node_id", (i64::MAX as u64) + 1).expect_err("should reject overflow");
        assert_eq!(
            err,
            QueryError::InputOutOfRange {
                argument: "node_id",
                value: (i64::MAX as u64) + 1,
            }
        );
    }

    #[test]
    fn transitive_impact_rejects_oversized_node_id() {
        let executor = RecordingExecutor::new(vec![]);
        let err = TransitiveImpact::new(&executor)
            .run(u64::MAX, None)
            .expect_err("u64::MAX exceeds i64");
        match err {
            RunError::Parse(QueryError::InputOutOfRange { argument, value }) => {
                assert_eq!(argument, "node_id");
                assert_eq!(value, u64::MAX);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            executor.take_calls().is_empty(),
            "executor must not be invoked on input validation failure"
        );
    }

    #[test]
    fn transitive_impact_zero_depth_short_circuits() {
        let executor = RecordingExecutor::new(vec![row(&[("node", CozoValue::Int(99))])]);
        let result = TransitiveImpact::new(&executor)
            .run(1, Some(0))
            .expect("zero depth returns empty");
        assert_eq!(result, Vec::<u64>::new());
        assert!(
            executor.take_calls().is_empty(),
            "executor must not be invoked when max_depth is 0"
        );
    }

    #[test]
    fn node_id_from_value_rejects_negative_int() {
        let err = node_id_from_value(&CozoValue::Int(-1), "node").expect_err("should reject");
        assert_eq!(
            err,
            QueryError::OutOfRange {
                column: "node",
                value: -1
            }
        );
    }

    #[test]
    fn node_id_from_value_rejects_non_int() {
        let err = node_id_from_value(&CozoValue::Null, "node").expect_err("should reject");
        assert_eq!(
            err,
            QueryError::UnexpectedType {
                column: "node",
                expected: "int"
            }
        );
    }

    #[test]
    fn query_error_display_includes_context() {
        assert_eq!(
            QueryError::MissingColumn("foo").to_string(),
            "missing column `foo` in query result"
        );
        assert_eq!(
            QueryError::UnexpectedType {
                column: "x",
                expected: "int"
            }
            .to_string(),
            "column `x` had unexpected type, expected int"
        );
        assert_eq!(
            QueryError::OutOfRange {
                column: "n",
                value: -7
            }
            .to_string(),
            "column `n` value -7 is out of range"
        );
        let display = QueryError::InputOutOfRange {
            argument: "node_id",
            value: u64::MAX,
        }
        .to_string();
        assert!(display.contains("node_id"));
        assert!(display.contains(&u64::MAX.to_string()));
    }

    #[test]
    fn run_error_display_wraps_inner() {
        let parse: RunError<String> = RunError::Parse(QueryError::MissingColumn("x"));
        assert!(parse.to_string().contains("missing column `x`"));

        let exec: RunError<String> = RunError::Executor("nope".to_string());
        assert!(exec.to_string().contains("nope"));
    }
}
