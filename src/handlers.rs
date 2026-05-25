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

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::cozo::CozoStore;
use crate::daemon::ConnectionHandler;
use crate::daemon_status::{DaemonStatus, RSS_WARNING_THRESHOLD_BYTES};
use crate::indexes::{HotIndexes, NodeId, SymbolKey};

pub struct WorktreeHandler {
    indexes: Arc<HotIndexes>,
    status: Arc<DaemonStatus>,
    /// Worktree root — used by the `node` tool to read source byte ranges.
    worktree_root: PathBuf,
    /// Cozo store — used by tools that need transitive graph queries
    /// (`impact`) which the in-memory hot index can't satisfy because it
    /// only tracks call edges, not inherits/implements/references.
    store: Arc<CozoStore>,
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
        }
    }

    /// Tool dispatch + per-tool `catching_up` scoping. Returns the result
    /// payload AND whether this specific request scope is mid-flight.
    fn dispatch(&self, request: &Request) -> Result<(Value, bool), RpcError> {
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
                let ids: Vec<String> = self
                    .indexes
                    .callers_of(&NodeId::from(params.node_id.as_str()))
                    .into_iter()
                    .map(|id| id.as_str().to_owned())
                    .collect();
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((json!({ "node_ids": ids }), catching_up))
            }
            "callees_of" => {
                let params: NodeIdParams = parse_params(request)?;
                let ids: Vec<String> = self
                    .indexes
                    .callees_of(&NodeId::from(params.node_id.as_str()))
                    .into_iter()
                    .map(|id| id.as_str().to_owned())
                    .collect();
                let catching_up = !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((json!({ "node_ids": ids }), catching_up))
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
                let catching_up = !self.status.is_reconcile_done()
                    || self.status.is_path_pending(&record.path);
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
                let paths: Vec<Value> = self
                    .indexes
                    .list_files()
                    .into_iter()
                    .map(|p| Value::String(p.to_string_lossy().into_owned()))
                    .collect();
                let catching_up =
                    !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((json!({ "files": paths }), catching_up))
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
                let catching_up =
                    !self.status.is_reconcile_done() || self.status.any_pending();
                Ok((json!({ "node_ids": affected }), catching_up))
            }
            other => Err(RpcError {
                code: -32601,
                message: format!("method not found: {other}"),
            }),
        }
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
        tokio::spawn(async move {
            let handler = WorktreeHandler {
                indexes,
                status,
                worktree_root,
                store,
            };
            let _ = serve_connection(&handler, conn).await;
        })
    }
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

#[derive(Debug, Deserialize)]
struct ImpactParams {
    node_id: String,
    /// Optional bound on transitive depth. `None` or `0` runs the
    /// unbounded variant.
    #[serde(default)]
    max_depth: Option<u32>,
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
    const MAX_SNIPPET_BYTES: u64 = 4096;
    let start = start?;
    let end = end?;
    if end <= start {
        return None;
    }
    let length = (end - start).min(MAX_SNIPPET_BYTES);
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

/// Backward transitive closure over `Calls`, `Inherits`, `Implements`, and
/// `References` (and their lowercase variants) edge kinds — every node
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
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'inherits'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'implements'
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'references'
affected[node] := impact_edge[node, $target]
affected[node] := affected[downstream], impact_edge[node, downstream]
?[node] := affected[node]
:sort node\n";
    let bounded = "\
impact_edge[from, to] := *edge[from, kind, to, _p, _c], kind = 'calls'
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
        params.insert(
            "max".into(),
            cozo::DataValue::from(i64::from(max_depth)),
        );
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

    fn empty_setup() -> (
        Arc<HotIndexes>,
        Arc<DaemonStatus>,
        PathBuf,
        Arc<CozoStore>,
    ) {
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
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handler = WorktreeHandler::new(indexes, status, worktree_root, store);

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
        // List is sorted for determinism.
        assert_eq!(files[0], "src/a.rs");
        assert_eq!(files[1], "src/b.rs");
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
}
