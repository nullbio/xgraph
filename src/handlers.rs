//! Daemon-side connection handler serving MCP-style RPCs out of `HotIndexes`.
//!
//! Each request is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"method":<name>,"params":{...}}`.
//! Each response is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"result":<value>,"meta":{...}}`
//! or `{"jsonrpc":"2.0","id":<n>,"error":{"code":<int>,"message":<str>},"meta":{...}}`.
//!
//! Hot lookups hit `HotIndexes` in memory — never Cozo. Complex graph
//! analyses (transitive impact, cycles) would use `crate::query` against
//! the Cozo store; those are not exposed by this handler yet.
//!
//! Every response carries a `meta` envelope with:
//! - `catching_up`: true if the daemon's view of the requested scope may be
//!   incomplete (initial reconcile still running, or a relevant path is
//!   mid-flight from the watcher).
//! - `rss_bytes`: resident-set size of the daemon process at response time,
//!   read from `/proc/self/statm` (cheap).
//! - `warnings`: an array of strings, populated when the daemon notices a
//!   pressure signal worth surfacing (e.g. `"high_memory_usage"` when RSS
//!   exceeds `daemon_status::RSS_WARNING_THRESHOLD_BYTES`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::cozo::CozoStore;
use crate::daemon::ConnectionHandler;
use crate::daemon_status::{DaemonStatus, RSS_WARNING_THRESHOLD_BYTES};
use crate::indexes::{HotIndexes, NodeId, SearchMode, SearchQuery, SymbolKey};
use crate::owner::{IndexSummary, MaintenanceCommand, MaintenanceSender};

pub struct WorktreeHandler {
    indexes: Arc<HotIndexes>,
    status: Arc<DaemonStatus>,
    /// Worktree root — used by the `node` tool to read source byte ranges.
    worktree_root: PathBuf,
    /// Cozo store — used by tools that need transitive graph queries
    /// (`impact`) which the in-memory hot index can't satisfy because it
    /// only tracks call edges, not inherits/implements/references.
    store: Arc<CozoStore>,
    maintenance: Option<MaintenanceSender>,
    maintenance_gate: Arc<RwLock<()>>,
}

impl WorktreeHandler {
    pub fn new(
        indexes: Arc<HotIndexes>,
        status: Arc<DaemonStatus>,
        worktree_root: PathBuf,
        store: Arc<CozoStore>,
    ) -> Self {
        Self {
            indexes,
            status,
            worktree_root,
            store,
            maintenance: None,
            maintenance_gate: Arc::new(RwLock::new(())),
        }
    }

    pub fn with_maintenance(
        indexes: Arc<HotIndexes>,
        status: Arc<DaemonStatus>,
        worktree_root: PathBuf,
        store: Arc<CozoStore>,
        maintenance: MaintenanceSender,
        maintenance_gate: Arc<RwLock<()>>,
    ) -> Self {
        Self {
            indexes,
            status,
            worktree_root,
            store,
            maintenance: Some(maintenance),
            maintenance_gate,
        }
    }

    /// Tool dispatch + per-tool `catching_up` scoping. Returns the result
    /// payload AND whether this specific request scope is mid-flight.
    fn dispatch(&self, request: &Request) -> Result<(Value, bool), RpcError> {
        match request.method.as_str() {
            "sync" => return self.dispatch_maintenance(MaintenanceOp::Sync),
            "reindex" => return self.dispatch_maintenance(MaintenanceOp::Reindex),
            _ => {}
        }

        let _maintenance_read = self.maintenance_gate.read();
        self.dispatch_read(request)
    }

    fn dispatch_read(&self, request: &Request) -> Result<(Value, bool), RpcError> {
        match request.method.as_str() {
            "find_symbol" => {
                let params: FindSymbolParams = parse_params(request)?;
                let ids = match &params.kind {
                    Some(kind) => self.indexes.lookup_symbol(&SymbolKey {
                        name: params.name.clone(),
                        kind: kind.clone(),
                    }),
                    None => self.indexes.lookup_symbol_by_name(&params.name),
                };
                let hits: Vec<Value> = ids
                    .into_iter()
                    .map(|id| {
                        let record = self.indexes.get_node(&id);
                        json!({
                            "node_id": id.as_str(),
                            "qname": record.as_ref().map(|r| r.qname.as_str()).unwrap_or(""),
                            "kind": record.as_ref().map(|r| r.kind.as_str()).unwrap_or(""),
                            "path": record.as_ref().map(|r| r.path.to_string_lossy().into_owned()).unwrap_or_default(),
                        })
                    })
                    .collect();
                // Symbol queries span the whole graph; a pending file might
                // define the symbol the caller asked about. Conservative:
                // catching_up if anything is pending or the initial reconcile
                // hasn't finished yet.
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((json!({ "hits": hits }), catching_up))
            }
            "callers_of" => {
                let params: NodeIdParams = parse_params(request)?;
                let nodes = self
                    .indexes
                    .callers_of(&NodeId::from(params.node_id.as_str()))
                    .into_iter()
                    .map(|id| self.summarize_node(&id))
                    .collect::<Vec<_>>();
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                // `node_ids` kept as a plain array of strings so older CLI
                // clients (and `xgraph callers` pretty-printing) still
                // work; `nodes` is the agent-friendly format.
                let ids: Vec<&str> = nodes
                    .iter()
                    .filter_map(|n| n.get("node_id").and_then(|v| v.as_str()))
                    .collect();
                Ok((json!({ "node_ids": ids, "nodes": nodes }), catching_up))
            }
            "callees_of" => {
                let params: NodeIdParams = parse_params(request)?;
                let nodes = self
                    .indexes
                    .callees_of(&NodeId::from(params.node_id.as_str()))
                    .into_iter()
                    .map(|id| self.summarize_node(&id))
                    .collect::<Vec<_>>();
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                let ids: Vec<&str> = nodes
                    .iter()
                    .filter_map(|n| n.get("node_id").and_then(|v| v.as_str()))
                    .collect();
                Ok((json!({ "node_ids": ids, "nodes": nodes }), catching_up))
            }
            "nodes_in_file" => {
                let params: NodesInFileParams = parse_params(request)?;
                let path_buf = PathBuf::from(&params.path);
                let ids = self.indexes.nodes_in_file(&path_buf);
                let nodes: Vec<Value> = ids
                    .into_iter()
                    .map(|id| {
                        let record = self.indexes.get_node(&id);
                        json!({
                            "node_id": id.as_str(),
                            "kind": record.as_ref().map(|r| r.kind.as_str()).unwrap_or(""),
                            "name": record.as_ref().map(|r| r.name.as_str()).unwrap_or(""),
                            "qname": record.as_ref().map(|r| r.qname.as_str()).unwrap_or(""),
                        })
                    })
                    .collect();
                // File-scoped: precise. The result is potentially stale iff
                // this specific path is mid-flight OR the initial reconcile
                // is still scanning.
                let catching_up =
                    !self.status.is_reconcile_done() || self.status.is_path_pending(&path_buf);
                Ok((json!({ "nodes": nodes }), catching_up))
            }
            "node" => {
                let params: NodeIdParams = parse_params(request)?;
                let id = NodeId::from(params.node_id.as_str());
                let record = match self.indexes.get_node(&id) {
                    Some(r) => r,
                    None => {
                        return Ok((json!({ "node": null }), false));
                    }
                };
                // Pull span + source snippet via Cozo. The span is stored
                // as a `[start_byte, end_byte, start_row, start_col]` list.
                let mut span_start: Option<u64> = None;
                let mut span_end: Option<u64> = None;
                let mut span_row: Option<u64> = None;
                let mut span_col: Option<u64> = None;
                if let Ok(rows) = self.store.run_read(
                    "?[span] := *active_node[$id, _path, _hash, _local, _kind, _name, _qname, span]",
                    [(
                        "id".to_string(),
                        cozo::DataValue::from(record.id.as_str()),
                    )]
                    .into(),
                ) && let Some(row) = rows.rows.into_iter().next()
                    && let Some(cozo::DataValue::List(span_list)) = row.into_iter().next()
                {
                    let mut it = span_list.into_iter();
                    span_start = it.next().and_then(data_to_u64);
                    span_end = it.next().and_then(data_to_u64);
                    span_row = it.next().and_then(data_to_u64);
                    span_col = it.next().and_then(data_to_u64);
                }
                let source_snippet =
                    read_snippet(&self.worktree_root, &record.path, span_start, span_end);
                let catching_up =
                    !self.status.is_reconcile_done() || self.status.is_path_pending(&record.path);
                Ok((
                    json!({
                        "node": {
                            "node_id": record.id.as_str(),
                            "path": record.path.to_string_lossy(),
                            "kind": record.kind,
                            "name": record.name,
                            "qname": record.qname,
                            "span": {
                                "start_byte": span_start,
                                "end_byte": span_end,
                                "start_row": span_row,
                                "start_col": span_col,
                            },
                            "source": source_snippet,
                        }
                    }),
                    catching_up,
                ))
            }
            "files" => {
                let params: FilesParams = parse_params(request)?;
                let all_paths = self
                    .indexes
                    .list_files()
                    .into_iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|p| {
                        params
                            .prefix
                            .as_deref()
                            .is_none_or(|prefix| p.starts_with(prefix))
                    })
                    .collect::<Vec<_>>();
                let total = all_paths.len();
                let offset = params.offset.unwrap_or(0).min(total);
                let default_limit = total.saturating_sub(offset);
                let limit = params.limit.unwrap_or(default_limit).min(10_000);
                let returned = all_paths
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .collect::<Vec<_>>();
                let paths: Vec<Value> = returned.into_iter().map(Value::String).collect();
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((
                    json!({
                        "files": paths,
                        "total": total,
                        "offset": offset,
                        "limit": limit,
                    }),
                    catching_up,
                ))
            }
            "status" => {
                let catching_up = !self.status.is_reconcile_done();
                self.status.refresh_rss();
                Ok((
                    json!({
                        "files": self.indexes.file_count(),
                        "nodes": self.indexes.node_count(),
                        "symbols": self.indexes.symbol_count(),
                        "call_edges": self.indexes.call_edge_count(),
                        "rss_bytes": self.status.rss_bytes(),
                        "pending_paths": self.status.pending_count(),
                        "reconcile_done": self.status.is_reconcile_done(),
                    }),
                    catching_up,
                ))
            }
            "impact" => {
                let params: ImpactParams = parse_params(request)?;
                let max_depth = params.max_depth.unwrap_or(0);
                let affected = run_impact_query(&self.store, &params.node_id, max_depth)?;
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((json!({ "node_ids": affected }), catching_up))
            }
            "search" => {
                let params: SearchParams = parse_params(request)?;
                let query = SearchQuery {
                    name: params.name,
                    mode: match params.mode.as_deref() {
                        Some("prefix") => SearchMode::Prefix,
                        Some("contains") => SearchMode::Contains,
                        _ => SearchMode::Exact,
                    },
                    kind: params.kind,
                    path_prefix: params.path_prefix,
                    limit: params.limit.unwrap_or(64).min(1024),
                };
                let ids = self.indexes.search(&query);
                let hits: Vec<Value> = ids
                    .into_iter()
                    .map(|id| {
                        let record = self.indexes.get_node(&id);
                        json!({
                            "node_id": id.as_str(),
                            "name": record.as_ref().map(|r| r.name.as_str()).unwrap_or(""),
                            "kind": record.as_ref().map(|r| r.kind.as_str()).unwrap_or(""),
                            "qname": record.as_ref().map(|r| r.qname.as_str()).unwrap_or(""),
                            "path": record.as_ref().map(|r| r.path.to_string_lossy().into_owned()).unwrap_or_default(),
                        })
                    })
                    .collect();
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((json!({ "hits": hits }), catching_up))
            }
            "context" => {
                let params: ContextParams = parse_params(request)?;
                let (payload, catching_up) = self.build_context(params);
                Ok((payload, catching_up))
            }
            "explore" => {
                let params: ExploreParams = parse_params(request)?;
                let (payload, catching_up) = self.build_explore(params);
                Ok((payload, catching_up))
            }
            "trace" => {
                let params: TraceParams = parse_params(request)?;
                let payload = self.build_trace(params);
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((payload, catching_up))
            }
            other => Err(RpcError {
                code: -32601,
                message: format!("method not found: {other}"),
            }),
        }
    }

    fn dispatch_maintenance(&self, op: MaintenanceOp) -> Result<(Value, bool), RpcError> {
        let Some(sender) = &self.maintenance else {
            return Err(RpcError {
                code: -32601,
                message: "maintenance commands are not available on this handler".to_string(),
            });
        };
        let (cmd, rx) = match op {
            MaintenanceOp::Sync => MaintenanceCommand::sync(),
            MaintenanceOp::Reindex => MaintenanceCommand::reindex(),
        };
        sender.send(cmd).map_err(|_| RpcError {
            code: -32000,
            message: "maintenance worker is not running".to_string(),
        })?;
        let summary = rx.recv().map_err(|_| RpcError {
            code: -32000,
            message: "maintenance worker stopped before replying".to_string(),
        })?;
        match summary {
            Ok(summary) => Ok((self.maintenance_summary_json(op, summary), false)),
            Err(err) => Err(RpcError {
                code: -32000,
                message: err.to_string(),
            }),
        }
    }

    fn maintenance_summary_json(&self, op: MaintenanceOp, summary: IndexSummary) -> Value {
        json!({
            "operation": match op {
                MaintenanceOp::Sync => "sync",
                MaintenanceOp::Reindex => "reindex",
            },
            "files_scanned": summary.files_scanned,
            "files_indexed": summary.files_indexed,
            "nodes_created": summary.nodes_created,
            "edges_created": summary.edges_created,
            "graph": {
                "files": self.indexes.file_count(),
                "nodes": self.indexes.node_count(),
                "symbols": self.indexes.symbol_count(),
                "call_edges": self.indexes.call_edge_count(),
            },
            "timings": {
                "scan_us": summary.timings.scan_us,
                "parse_us": summary.timings.parse_us,
                "resolve_us": summary.timings.resolve_us,
                "store_us": summary.timings.store_us,
            },
        })
    }

    /// Compose `find_symbol` + `node` + `callers_of` + `callees_of` into
    /// a single payload so an agent can prime task context in one call.
    /// Avoids the 4× round-trip cost of issuing these tools separately.
    fn build_context(&self, params: ContextParams) -> (Value, bool) {
        let related_limit = params.related_limit.unwrap_or(20).min(200);
        let snippet_bytes = params.snippet_bytes.unwrap_or(2048).min(8192);
        let match_limit = params.limit.unwrap_or(20).min(200);

        let ids = match &params.kind {
            Some(k) => self.indexes.lookup_symbol(&SymbolKey {
                name: params.name.clone(),
                kind: k.clone(),
            }),
            None => self.indexes.lookup_symbol_by_name(&params.name),
        };

        let mut matches: Vec<Value> = Vec::with_capacity(ids.len().min(match_limit));
        let mut total_matches = 0usize;
        for id in &ids {
            let Some(record) = self.indexes.get_node(id) else {
                continue;
            };
            if params
                .path_prefix
                .as_deref()
                .is_some_and(|prefix| !record.path.to_string_lossy().starts_with(prefix))
            {
                continue;
            }
            total_matches += 1;
            if matches.len() >= match_limit {
                continue;
            }
            let (span, source) = self.lookup_span_and_snippet(id, &record.path, snippet_bytes);
            let caller_nodes: Vec<Value> = self
                .indexes
                .callers_of(id)
                .into_iter()
                .take(related_limit)
                .map(|c| self.summarize_node(&c))
                .collect();
            let callee_nodes: Vec<Value> = self
                .indexes
                .callees_of(id)
                .into_iter()
                .take(related_limit)
                .map(|c| self.summarize_node(&c))
                .collect();
            let callers: Vec<String> = caller_nodes
                .iter()
                .filter_map(|node| {
                    node.get("node_id")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned)
                })
                .collect();
            let callees: Vec<String> = callee_nodes
                .iter()
                .filter_map(|node| {
                    node.get("node_id")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned)
                })
                .collect();
            matches.push(json!({
                "node_id": id.as_str(),
                "name": record.name,
                "kind": record.kind,
                "qname": record.qname,
                "path": record.path.to_string_lossy(),
                "span": span,
                "source": source,
                "callers": callers,
                "callees": callees,
                "caller_nodes": caller_nodes,
                "callee_nodes": callee_nodes,
            }));
        }

        let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
        (
            json!({ "matches": matches, "total_matches": total_matches, "limit": match_limit }),
            catching_up,
        )
    }

    /// Read source snippets for a batch of node ids, capped to a total
    /// byte budget. Used by `explore` to surface multiple symbols at once
    /// without unbounded payload sizes.
    fn build_explore(&self, params: ExploreParams) -> (Value, bool) {
        let total_budget = params.byte_budget.unwrap_or(32 * 1024).min(128 * 1024);
        let per_snippet = params.per_snippet_bytes.unwrap_or(4096).min(16 * 1024);
        let mut remaining = total_budget;

        let mut items: Vec<Value> = Vec::with_capacity(params.node_ids.len());
        let mut catching_up = !self.status.is_reconcile_done();
        for raw_id in &params.node_ids {
            let id = NodeId::from(raw_id.as_str());
            let Some(record) = self.indexes.get_node(&id) else {
                items.push(json!({ "node_id": raw_id, "node": null }));
                continue;
            };
            if self.status.is_path_pending(&record.path) {
                catching_up = true;
            }
            let snippet_cap = per_snippet.min(remaining);
            let (span, source) = self.lookup_span_and_snippet(&id, &record.path, snippet_cap);
            if let Some(ref s) = source {
                remaining = remaining.saturating_sub(s.len());
            }
            items.push(json!({
                "node_id": raw_id,
                "name": record.name,
                "kind": record.kind,
                "qname": record.qname,
                "path": record.path.to_string_lossy(),
                "span": span,
                "source": source,
            }));
            if remaining == 0 {
                break;
            }
        }

        (
            json!({
                "items": items,
                "bytes_used": total_budget - remaining,
                "bytes_budget": total_budget,
            }),
            catching_up,
        )
    }

    /// Bidirectional BFS over the in-memory call graph from `from` to
    /// `to`. Falls back to forward BFS if the bidirectional search would
    /// hit zero overlap (e.g., target is unreachable). Returns the
    /// shortest path of node ids or `null` if none exists within
    /// `max_depth`.
    ///
    /// Performance: pure in-memory traversal off `HotIndexes` — no Cozo
    /// round-trip. `BTreeMap`s would keep the work-set sorted but cost
    /// O(log n) per insertion; `HashMap` + `VecDeque` is faster here.
    fn build_trace(&self, params: TraceParams) -> Value {
        use std::collections::{HashMap, VecDeque};
        let from = NodeId::from(params.from.as_str());
        let to = NodeId::from(params.to.as_str());
        let max_depth = params.max_depth.unwrap_or(12).min(64);

        if from == to {
            return json!({
                "path": [self.node_for_trace(&from)],
                "length": 0,
            });
        }

        // Forward visited: caller -> parent (the node we expanded from).
        let mut forward_parent: HashMap<NodeId, Option<NodeId>> = HashMap::new();
        forward_parent.insert(from.clone(), None);
        let mut forward_queue: VecDeque<(NodeId, usize)> = VecDeque::new();
        forward_queue.push_back((from.clone(), 0));

        while let Some((node, depth)) = forward_queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for callee in self.indexes.callees_of(&node) {
                if !forward_parent.contains_key(&callee) {
                    forward_parent.insert(callee.clone(), Some(node.clone()));
                    if callee == to {
                        return json!({
                            "path": self.reconstruct_path(&forward_parent, &to),
                            "length": depth + 1,
                        });
                    }
                    forward_queue.push_back((callee, depth + 1));
                }
            }
        }
        json!({ "path": null, "length": null })
    }

    fn reconstruct_path(
        &self,
        parent: &std::collections::HashMap<NodeId, Option<NodeId>>,
        target: &NodeId,
    ) -> Vec<Value> {
        let mut chain: Vec<NodeId> = Vec::new();
        let mut cur = Some(target.clone());
        while let Some(node) = cur {
            chain.push(node.clone());
            cur = parent.get(&node).and_then(|p| p.clone());
        }
        chain.reverse();
        chain.iter().map(|n| self.node_for_trace(n)).collect()
    }

    /// Compact JSON for "here's a node id, give me just enough to know
    /// what it is": id + name + kind + qname + path. Used by
    /// `callers_of` / `callees_of` so an agent doesn't have to fan out
    /// to `node` / `explore` to learn what each id refers to.
    fn summarize_node(&self, id: &NodeId) -> Value {
        match self.indexes.get_node(id) {
            Some(r) => json!({
                "node_id": id.as_str(),
                "name": r.name,
                "kind": r.kind,
                "qname": r.qname,
                "path": r.path.to_string_lossy(),
            }),
            None => json!({ "node_id": id.as_str() }),
        }
    }

    fn node_for_trace(&self, id: &NodeId) -> Value {
        match self.indexes.get_node(id) {
            Some(r) => json!({
                "node_id": id.as_str(),
                "name": r.name,
                "kind": r.kind,
                "qname": r.qname,
                "path": r.path.to_string_lossy(),
            }),
            None => json!({ "node_id": id.as_str(), "name": null }),
        }
    }

    /// Fetch the `span` array + a bounded source snippet for a node id.
    /// Returns `(span_json, snippet)` where either may be null if the
    /// active_node row is missing or the file is unreadable.
    fn lookup_span_and_snippet(
        &self,
        id: &NodeId,
        path: &std::path::Path,
        max_bytes: usize,
    ) -> (Value, Option<String>) {
        let mut span_start: Option<u64> = None;
        let mut span_end: Option<u64> = None;
        let mut span_row: Option<u64> = None;
        let mut span_col: Option<u64> = None;
        if let Ok(rows) = self.store.run_read(
            "?[span] := *active_node[$id, _path, _hash, _local, _kind, _name, _qname, span]",
            [("id".to_string(), cozo::DataValue::from(id.as_str()))].into(),
        ) && let Some(row) = rows.rows.into_iter().next()
            && let Some(cozo::DataValue::List(span_list)) = row.into_iter().next()
        {
            let mut it = span_list.into_iter();
            span_start = it.next().and_then(data_to_u64);
            span_end = it.next().and_then(data_to_u64);
            span_row = it.next().and_then(data_to_u64);
            span_col = it.next().and_then(data_to_u64);
        }
        let snippet = if max_bytes > 0 {
            read_snippet_with_cap(&self.worktree_root, path, span_start, span_end, max_bytes)
        } else {
            None
        };
        (
            json!({
                "start_byte": span_start,
                "end_byte": span_end,
                "start_row": span_row,
                "start_col": span_col,
            }),
            snippet,
        )
    }

    fn build_meta(&self, catching_up: bool) -> Value {
        // Refresh memory probe once per response. /proc/self/statm is a
        // virtual file, ~30 bytes; the read is one syscall + a tiny parse.
        self.status.refresh_rss();
        let rss = self.status.rss_bytes();
        let mut warnings = Vec::new();
        if rss > RSS_WARNING_THRESHOLD_BYTES {
            warnings.push(json!({
                "kind": "high_memory_usage",
                "rss_bytes": rss,
                "threshold_bytes": RSS_WARNING_THRESHOLD_BYTES,
            }));
        }
        json!({
            "catching_up": catching_up,
            "rss_bytes": rss,
            "pending_paths": self.status.pending_count(),
            "warnings": warnings,
        })
    }
}

impl ConnectionHandler for WorktreeHandler {
    fn handle(&self, conn: UnixStream) -> JoinHandle<()> {
        let indexes = Arc::clone(&self.indexes);
        let status = Arc::clone(&self.status);
        let worktree_root = self.worktree_root.clone();
        let store = Arc::clone(&self.store);
        let maintenance = self.maintenance.clone();
        let maintenance_gate = Arc::clone(&self.maintenance_gate);
        tokio::spawn(async move {
            let handler = WorktreeHandler {
                indexes,
                status,
                worktree_root,
                store,
                maintenance,
                maintenance_gate,
            };
            let _ = serve_connection(&handler, conn).await;
        })
    }
}

#[derive(Clone, Copy)]
enum MaintenanceOp {
    Sync,
    Reindex,
}

async fn serve_connection(handler: &WorktreeHandler, conn: UnixStream) -> std::io::Result<()> {
    let (read_half, mut write_half) = conn.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => match handler.dispatch(&req) {
                Ok((result, catching_up)) => json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "result": result,
                    "meta": handler.build_meta(catching_up),
                }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "error": { "code": err.code, "message": err.message },
                    // Errors still attach meta so clients can correlate
                    // backpressure with failures.
                    "meta": handler.build_meta(false),
                }),
            },
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("parse error: {err}") },
                "meta": handler.build_meta(false),
            }),
        };
        let serialized = response.to_string();
        write_half.write_all(serialized.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
    }
}

#[derive(Debug, Deserialize)]
struct Request {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct FindSymbolParams {
    name: String,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NodeIdParams {
    node_id: String,
}

#[derive(Debug, Deserialize)]
struct NodesInFileParams {
    path: String,
}

#[derive(Debug, Deserialize, Default)]
struct FilesParams {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ImpactParams {
    node_id: String,
    /// Optional bound on transitive depth. `None` or `0` runs the
    /// unbounded variant.
    #[serde(default)]
    max_depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    name: String,
    /// One of `exact` (default), `prefix`, `contains`.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    path_prefix: Option<String>,
    /// Hard cap on returned hits. Defaults to 64; max 1024.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ContextParams {
    /// Search query for the symbol that frames the context.
    name: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    path_prefix: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    /// How many caller / callee ids to include per direction.
    #[serde(default)]
    related_limit: Option<usize>,
    /// Cap on the snippet returned for the primary symbol.
    #[serde(default)]
    snippet_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ExploreParams {
    /// Node ids to expand into source snippets. Order is preserved.
    node_ids: Vec<String>,
    /// Total byte budget across all snippets. Defaults to 32 KiB.
    #[serde(default)]
    byte_budget: Option<usize>,
    /// Per-snippet cap so a single huge function doesn't consume the
    /// whole budget. Defaults to 4 KiB.
    #[serde(default)]
    per_snippet_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TraceParams {
    from: String,
    to: String,
    /// Hard bound on BFS depth. Defaults to 12 — beyond that, paths are
    /// rarely insightful and the search starts to dominate latency.
    #[serde(default)]
    max_depth: Option<usize>,
}

/// Decode a Cozo `Int` value into `u64` for span fields. Returns `None`
/// when the value is missing or has the wrong shape; callers fall back to
/// an absent span in the response payload.
fn data_to_u64(v: cozo::DataValue) -> Option<u64> {
    match v {
        cozo::DataValue::Num(cozo::Num::Int(i)) => i.try_into().ok(),
        _ => None,
    }
}

/// Read a `[start_byte, end_byte)` slice from `worktree_root.join(relative)`.
/// Caps the snippet at 4 KiB to keep MCP responses bounded; clients that
/// need the full source can `read` the file themselves. Returns `None` on
/// any I/O error so the response simply omits the snippet.
fn read_snippet(
    worktree_root: &std::path::Path,
    relative: &std::path::Path,
    start: Option<u64>,
    end: Option<u64>,
) -> Option<String> {
    read_snippet_with_cap(worktree_root, relative, start, end, 4096)
}

/// Same as [`read_snippet`] with a caller-supplied byte cap. Used by
/// `context` / `explore` which apportion a shared output budget across
/// many snippets.
fn read_snippet_with_cap(
    worktree_root: &std::path::Path,
    relative: &std::path::Path,
    start: Option<u64>,
    end: Option<u64>,
    cap_bytes: usize,
) -> Option<String> {
    let start = start?;
    let end = end?;
    if end <= start || cap_bytes == 0 {
        return None;
    }
    let length = (end - start).min(cap_bytes as u64);
    let full = worktree_root.join(relative);
    let bytes = std::fs::read(&full).ok()?;
    let start_usize: usize = start.try_into().ok()?;
    let length_usize: usize = length.try_into().ok()?;
    let end_usize = start_usize.checked_add(length_usize)?.min(bytes.len());
    if start_usize >= bytes.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes[start_usize..end_usize]).into_owned())
}

/// Backward transitive closure over calls, renders, inheritance, implementations,
/// and references edge kinds — every node
/// whose behavior may be affected if the target changes. Inline Datalog
/// because `query.rs` types use `u64` ids while our edge source/target
/// columns are strings.
fn run_impact_query(
    store: &CozoStore,
    node_id: &str,
    max_depth: u32,
) -> Result<Vec<String>, RpcError> {
    // Lowercase edge kinds match what `owner::edge_kind_for_ref` emits.
    // The `edge` relation has 5 columns (source, kind, target, provenance,
    // confidence); the wildcards `_p, _c` bind the two we don't filter on.
    let unbounded = "\
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'calls'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'renders'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'inherits'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'implements'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'references'
affected[node] := impact_edge[node, $target]
affected[node] := affected[downstream], impact_edge[node, downstream]
?[node] := affected[node]
:sort node\n";
    let bounded = "\
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'calls'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'renders'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'inherits'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'implements'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'references'
affected[node, depth] := impact_edge[node, $target], depth = 1
affected[node, depth] := affected[downstream, prev], prev < $max, \
                        impact_edge[node, downstream], depth = prev + 1
?[node] := affected[node, _]
:sort node\n";

    let mut params: BTreeMap<String, cozo::DataValue> = BTreeMap::new();
    params.insert("target".into(), cozo::DataValue::from(node_id));
    let script = if max_depth == 0 {
        unbounded
    } else {
        params.insert("max".into(), cozo::DataValue::from(i64::from(max_depth)));
        bounded
    };
    let rows = store.run_read(script, params).map_err(|err| RpcError {
        code: -32603,
        message: format!("impact query failed: {err}"),
    })?;
    let mut out: Vec<String> = rows
        .rows
        .into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(cozo::DataValue::Str(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

fn parse_params<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, RpcError> {
    // serde_json supports deserializing from a `Value` reference via
    // `T::deserialize(&value)`, avoiding the per-request clone of the whole
    // params subtree that `from_value` would do.
    T::deserialize(&request.params).map_err(|err| RpcError {
        code: -32602,
        message: format!("invalid params: {err}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexes::{NodeRecord, SymbolKey};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn empty_setup() -> (Arc<HotIndexes>, Arc<DaemonStatus>, PathBuf, Arc<CozoStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let cozo_dir = tmp.path().join("cozo");
        std::fs::create_dir_all(&cozo_dir).unwrap();
        let store = Arc::new(CozoStore::open(&cozo_dir).unwrap());
        // Keep the tempdir alive for the duration of the test by leaking it —
        // the path is only used by `store` which already opened the DB.
        let worktree_root = tmp.keep();
        (
            Arc::new(HotIndexes::new()),
            Arc::new(DaemonStatus::new()),
            worktree_root,
            store,
        )
    }

    async fn run_request(
        indexes: Arc<HotIndexes>,
        status: Arc<DaemonStatus>,
        worktree_root: PathBuf,
        store: Arc<CozoStore>,
        request: &str,
    ) -> Value {
        run_request_with_handler(
            WorktreeHandler::new(indexes, status, worktree_root, store),
            request,
        )
        .await
    }

    async fn run_request_with_handler(handler: WorktreeHandler, request: &str) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            serve_connection(&handler, conn).await.unwrap();
        });

        let client = UnixStream::connect(&socket_path).await.unwrap();
        let (read, mut write) = client.into_split();
        write.write_all(request.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();
        write.shutdown().await.unwrap();
        drop(write);

        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let _ = server.await;
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn reindex_request_runs_through_maintenance_channel() {
        let (indexes, status, root, store) = empty_setup();
        let (tx, rx) = crate::owner::maintenance_channel();
        let gate = Arc::new(RwLock::new(()));
        let worker = std::thread::spawn(move || {
            let command = rx.recv().expect("maintenance command");
            match command {
                crate::owner::MaintenanceCommand::Reindex { reply } => {
                    let _ = reply.send(Ok(IndexSummary {
                        files_indexed: 2,
                        nodes_created: 3,
                        edges_created: 4,
                        ..IndexSummary::default()
                    }));
                }
                other => panic!("expected reindex, got {other:?}"),
            }
        });

        let handler = WorktreeHandler::with_maintenance(indexes, status, root, store, tx, gate);
        let resp = run_request_with_handler(
            handler,
            r#"{"jsonrpc":"2.0","id":31,"method":"reindex","params":{}}"#,
        )
        .await;
        worker.join().expect("maintenance worker joins");

        assert_eq!(resp["id"], 31);
        assert_eq!(resp["result"]["operation"], "reindex");
        assert_eq!(resp["result"]["files_indexed"], 2);
        assert_eq!(resp["result"]["nodes_created"], 3);
        assert_eq!(resp["result"]["edges_created"], 4);
    }

    #[tokio::test]
    async fn find_symbol_returns_empty_for_unknown() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":1,"method":"find_symbol","params":{"name":"Nothing"}}"#,
        )
        .await;
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["hits"].as_array().unwrap().len(), 0);
        assert_eq!(resp["meta"]["catching_up"], false);
        assert!(resp["meta"]["rss_bytes"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn meta_reports_catching_up_before_initial_reconcile() {
        let (indexes, status, root, store) = empty_setup();
        // status.mark_reconcile_done() intentionally NOT called.
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":2,"method":"find_symbol","params":{"name":"X"}}"#,
        )
        .await;
        assert_eq!(resp["meta"]["catching_up"], true);
    }

    #[tokio::test]
    async fn meta_catching_up_is_per_file_for_nodes_in_file() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        status.mark_pending(std::path::Path::new("src/dirty.rs"));
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":3,"method":"nodes_in_file","params":{"path":"src/clean.rs"}}"#,
        )
        .await;
        assert_eq!(
            resp["meta"]["catching_up"], false,
            "querying a clean path should not be catching up"
        );
    }

    #[tokio::test]
    async fn meta_catching_up_true_when_queried_path_is_pending() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        status.mark_pending(std::path::Path::new("src/dirty.rs"));
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":4,"method":"nodes_in_file","params":{"path":"src/dirty.rs"}}"#,
        )
        .await;
        assert_eq!(resp["meta"]["catching_up"], true);
    }

    #[tokio::test]
    async fn find_symbol_returns_registered_node() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        let id = NodeId::from("h:42");
        indexes.insert_node(NodeRecord {
            id: id.clone(),
            path: PathBuf::from("src/foo.rs"),
            kind: "class".to_string(),
            name: "User".to_string(),
            qname: "App\\Models\\User".to_string(),
        });
        indexes.register_symbol(
            SymbolKey {
                name: "User".to_string(),
                kind: "class".to_string(),
            },
            id.clone(),
        );

        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":7,"method":"find_symbol","params":{"name":"User"}}"#,
        )
        .await;
        let hits = resp["result"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["node_id"], "h:42");
        assert_eq!(hits[0]["qname"], "App\\Models\\User");
    }

    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error_with_meta() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":9,"method":"bogus","params":{}}"#,
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);
        assert!(resp["meta"]["rss_bytes"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn status_tool_reports_counts() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        let id = NodeId::from("h:1");
        indexes.insert_node(NodeRecord {
            id: id.clone(),
            path: PathBuf::from("src/a.rs"),
            kind: "function".to_string(),
            name: "f".to_string(),
            qname: "f".to_string(),
        });
        indexes.insert_file(PathBuf::from("src/a.rs"), vec![id]);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":10,"method":"status","params":{}}"#,
        )
        .await;
        assert_eq!(resp["result"]["files"], 1);
        assert_eq!(resp["result"]["nodes"], 1);
        assert_eq!(resp["result"]["reconcile_done"], true);
    }

    #[tokio::test]
    async fn files_tool_lists_indexed_paths() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        indexes.insert_file(PathBuf::from("src/a.rs"), vec![]);
        indexes.insert_file(PathBuf::from("src/b.rs"), vec![]);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":11,"method":"files","params":{}}"#,
        )
        .await;
        let files = resp["result"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(resp["result"]["total"], 2);
        assert_eq!(resp["result"]["offset"], 0);
        // List is sorted for determinism.
        assert_eq!(files[0], "src/a.rs");
        assert_eq!(files[1], "src/b.rs");
    }

    #[tokio::test]
    async fn files_tool_filters_and_pages_paths() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        indexes.insert_file(PathBuf::from("app/Actions/A.php"), vec![]);
        indexes.insert_file(PathBuf::from("app/Services/A.php"), vec![]);
        indexes.insert_file(PathBuf::from("app/Services/B.php"), vec![]);
        indexes.insert_file(PathBuf::from("tests/Feature/A.php"), vec![]);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":12,"method":"files","params":{"prefix":"app/Services","offset":1,"limit":1}}"#,
        )
        .await;
        let files = resp["result"]["files"].as_array().unwrap();
        assert_eq!(resp["result"]["total"], 2);
        assert_eq!(resp["result"]["offset"], 1);
        assert_eq!(resp["result"]["limit"], 1);
        assert_eq!(files, &vec![serde_json::json!("app/Services/B.php")]);
    }

    #[tokio::test]
    async fn node_tool_returns_record_for_known_id() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        let id = NodeId::from("h:99");
        indexes.insert_node(NodeRecord {
            id: id.clone(),
            path: PathBuf::from("src/x.rs"),
            kind: "class".to_string(),
            name: "X".to_string(),
            qname: "ns::X".to_string(),
        });
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":12,"method":"node","params":{"node_id":"h:99"}}"#,
        )
        .await;
        assert_eq!(resp["result"]["node"]["node_id"], "h:99");
        assert_eq!(resp["result"]["node"]["kind"], "class");
        assert_eq!(resp["result"]["node"]["qname"], "ns::X");
    }

    #[tokio::test]
    async fn node_tool_returns_null_for_unknown_id() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":13,"method":"node","params":{"node_id":"unknown"}}"#,
        )
        .await;
        assert!(resp["result"]["node"].is_null());
    }

    fn populate_demo_symbols(indexes: &HotIndexes) {
        for (name, kind, path) in [
            (
                "UserController",
                "class",
                "app/Http/Controllers/UserController.rs",
            ),
            ("UserService", "class", "app/Services/UserService.rs"),
            (
                "PostController",
                "class",
                "app/Http/Controllers/PostController.rs",
            ),
            ("handleRequest", "function", "lib/http.rs"),
        ] {
            let id = NodeId::from(format!("h:{name}"));
            indexes.insert_node(NodeRecord {
                id: id.clone(),
                path: PathBuf::from(path),
                kind: kind.to_string(),
                name: name.to_string(),
                qname: name.to_string(),
            });
            indexes.register_symbol(
                SymbolKey {
                    name: name.to_string(),
                    kind: kind.to_string(),
                },
                id,
            );
        }
    }

    #[tokio::test]
    async fn search_exact_returns_only_matching_symbol() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":20,"method":"search","params":{"name":"UserService"}}"#,
        )
        .await;
        let hits = resp["result"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["name"], "UserService");
    }

    #[tokio::test]
    async fn search_prefix_matches_all_user_symbols() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":21,"method":"search","params":{"name":"User","mode":"prefix"}}"#,
        )
        .await;
        let names: Vec<String> = resp["result"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["name"].as_str().unwrap_or("").to_owned())
            .collect();
        assert!(names.contains(&"UserController".to_owned()));
        assert!(names.contains(&"UserService".to_owned()));
        assert!(!names.contains(&"PostController".to_owned()));
    }

    #[tokio::test]
    async fn search_contains_finds_substring_match() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":22,"method":"search","params":{"name":"Controller","mode":"contains"}}"#,
        )
        .await;
        let hits = resp["result"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2, "expected User+Post controllers");
    }

    #[tokio::test]
    async fn search_filters_by_path_prefix() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":23,"method":"search","params":{"name":"User","mode":"prefix","path_prefix":"app/Services"}}"#,
        )
        .await;
        let hits = resp["result"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["name"], "UserService");
    }

    #[tokio::test]
    async fn search_respects_limit() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":24,"method":"search","params":{"name":"","mode":"contains","limit":2}}"#,
        )
        .await;
        let hits = resp["result"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn context_tool_returns_callers_and_callees() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let target = NodeId::from("h:UserService");
        let caller = NodeId::from("h:UserController");
        indexes.add_call_edge(caller, target);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":25,"method":"context","params":{"name":"UserService"}}"#,
        )
        .await;
        let matches = resp["result"]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        let callers = matches[0]["callers"].as_array().unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0], "h:UserController");
        let caller_nodes = matches[0]["caller_nodes"].as_array().unwrap();
        assert_eq!(caller_nodes[0]["qname"], "UserController");
        assert_eq!(resp["result"]["total_matches"], 1);
    }

    #[tokio::test]
    async fn context_tool_filters_by_path_prefix_and_limits_matches() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":29,"method":"context","params":{"name":"UserService","path_prefix":"app/Services","limit":1}}"#,
        )
        .await;
        let matches = resp["result"]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["path"], "app/Services/UserService.rs");
        assert_eq!(resp["result"]["total_matches"], 1);
        assert_eq!(resp["result"]["limit"], 1);
    }

    #[tokio::test]
    async fn explore_tool_returns_items_within_budget() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":26,"method":"explore","params":{"node_ids":["h:UserService","h:PostController"]}}"#,
        )
        .await;
        let items = resp["result"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(resp["result"]["bytes_budget"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn trace_tool_finds_direct_call_path() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let from = NodeId::from("h:UserController");
        let to = NodeId::from("h:UserService");
        indexes.add_call_edge(from.clone(), to.clone());
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":27,"method":"trace","params":{"from":"h:UserController","to":"h:UserService"}}"#,
        )
        .await;
        let path = resp["result"]["path"].as_array().unwrap();
        assert_eq!(path.len(), 2);
        assert_eq!(path[0]["node_id"], "h:UserController");
        assert_eq!(path[1]["node_id"], "h:UserService");
        assert_eq!(resp["result"]["length"], 1);
    }

    #[tokio::test]
    async fn trace_tool_returns_null_when_unreachable() {
        let (indexes, status, root, store) = empty_setup();
        status.mark_reconcile_done();
        populate_demo_symbols(&indexes);
        let resp = run_request(
            indexes,
            status,
            root,
            store,
            r#"{"jsonrpc":"2.0","id":28,"method":"trace","params":{"from":"h:UserController","to":"h:UserService"}}"#,
        )
        .await;
        assert!(resp["result"]["path"].is_null());
    }
}
