//! Daemon-side connection handler that serves a subset of MCP-style RPCs.
//!
//! Each request is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"method":<name>,"params":{...}}`.
//! Each response is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"result":<value>}` or
//! `{"jsonrpc":"2.0","id":<n>,"error":{"code":<int>,"message":<str>}}`.

use std::collections::BTreeMap;
use std::sync::Arc;

use cozo::DataValue;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::cozo::CozoStore;
use crate::daemon::ConnectionHandler;

pub struct WorktreeHandler {
    store: Arc<CozoStore>,
}

impl WorktreeHandler {
    pub fn new(store: Arc<CozoStore>) -> Self {
        Self { store }
    }

    fn dispatch(&self, request: &Request) -> Result<Value, RpcError> {
        match request.method.as_str() {
            "find_symbol" => {
                let params: FindSymbolParams = parse_params(request)?;
                find_symbol(&self.store, &params)
            }
            "callers_of" => {
                let params: NodeIdParams = parse_params(request)?;
                edges_to_node(&self.store, &params.node_id, "calls", Direction::Sources)
            }
            "callees_of" => {
                let params: NodeIdParams = parse_params(request)?;
                edges_to_node(&self.store, &params.node_id, "calls", Direction::Targets)
            }
            "nodes_in_file" => {
                let params: NodesInFileParams = parse_params(request)?;
                nodes_in_file(&self.store, &params.path)
            }
            other => Err(RpcError {
                code: -32601,
                message: format!("method not found: {other}"),
            }),
        }
    }
}

impl ConnectionHandler for WorktreeHandler {
    fn handle(&self, conn: UnixStream) -> JoinHandle<()> {
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            let handler = WorktreeHandler { store };
            let _ = serve_connection(&handler, conn).await;
        })
    }
}

enum Direction {
    Sources,
    Targets,
}

fn find_symbol(store: &CozoStore, params: &FindSymbolParams) -> Result<Value, RpcError> {
    let script = match &params.kind {
        Some(_) => {
            "?[node_id, qname, path] := *symbol[$name, $kind, node_id, qname, path] :sort node_id"
        }
        None => {
            "?[node_id, qname, path] := *symbol[$name, _kind, node_id, qname, path] :sort node_id"
        }
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("name".to_string(), DataValue::from(params.name.clone()));
    if let Some(k) = &params.kind {
        bindings.insert("kind".to_string(), DataValue::from(k.clone()));
    }
    let rows = store
        .run_read(script, bindings)
        .map_err(rpc_error_from_cozo)?;
    let hits: Vec<Value> = rows
        .rows
        .into_iter()
        .filter_map(|row| {
            let mut iter = row.into_iter();
            let node_id = data_to_string(iter.next()?)?;
            let qname = data_to_string(iter.next()?).unwrap_or_default();
            let path = data_to_string(iter.next()?).unwrap_or_default();
            Some(json!({
                "node_id": node_id,
                "qname": qname,
                "path": path,
            }))
        })
        .collect();
    Ok(json!({ "hits": hits }))
}

fn edges_to_node(
    store: &CozoStore,
    node_id: &str,
    kind: &str,
    direction: Direction,
) -> Result<Value, RpcError> {
    let script = match direction {
        Direction::Sources => {
            "?[other] := *edge[other, $kind, $node_id, _provenance, _confidence] :sort other"
        }
        Direction::Targets => {
            "?[other] := *edge[$node_id, $kind, other, _provenance, _confidence] :sort other"
        }
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("node_id".to_string(), DataValue::from(node_id.to_string()));
    bindings.insert("kind".to_string(), DataValue::from(kind.to_string()));
    let rows = store
        .run_read(script, bindings)
        .map_err(rpc_error_from_cozo)?;
    let ids: Vec<String> = rows
        .rows
        .into_iter()
        .filter_map(|row| {
            let mut iter = row.into_iter();
            data_to_string(iter.next()?)
        })
        .collect();
    Ok(json!({ "node_ids": ids }))
}

fn nodes_in_file(store: &CozoStore, path: &str) -> Result<Value, RpcError> {
    let script = "?[node_id, kind, name, qname] := \
        *active_node[node_id, $path, _hash, _local, kind, name, qname, _span] \
        :sort node_id";
    let mut bindings = BTreeMap::new();
    bindings.insert("path".to_string(), DataValue::from(path.to_string()));
    let rows = store
        .run_read(script, bindings)
        .map_err(rpc_error_from_cozo)?;
    let nodes: Vec<Value> = rows
        .rows
        .into_iter()
        .filter_map(|row| {
            let mut iter = row.into_iter();
            let node_id = data_to_string(iter.next()?)?;
            let kind = data_to_string(iter.next()?).unwrap_or_default();
            let name = data_to_string(iter.next()?).unwrap_or_default();
            let qname = data_to_string(iter.next()?).unwrap_or_default();
            Some(json!({
                "node_id": node_id,
                "kind": kind,
                "name": name,
                "qname": qname,
            }))
        })
        .collect();
    Ok(json!({ "nodes": nodes }))
}

fn data_to_string(value: DataValue) -> Option<String> {
    match value {
        DataValue::Str(s) => Some(s.into()),
        _ => None,
    }
}

fn rpc_error_from_cozo(err: crate::cozo::CozoError) -> RpcError {
    RpcError {
        code: -32000,
        message: err.to_string(),
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
                Ok(result) => json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "result": result,
                }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "error": { "code": err.code, "message": err.message },
                }),
            },
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("parse error: {err}") },
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

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

fn parse_params<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, RpcError> {
    serde_json::from_value(request.params.clone()).map_err(|err| RpcError {
        code: -32602,
        message: format!("invalid params: {err}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn open_test_store() -> (TempDir, Arc<CozoStore>) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("store");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CozoStore::open(&dir).expect("cozo open"));
        (tmp, store)
    }

    async fn run_request(store: Arc<CozoStore>, request: &str) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handler = WorktreeHandler::new(store);

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
        let (_tmp, store) = open_test_store();
        let resp = run_request(
            store,
            r#"{"jsonrpc":"2.0","id":1,"method":"find_symbol","params":{"name":"Nothing"}}"#,
        )
        .await;
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["hits"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn nodes_in_file_returns_empty_for_unknown_path() {
        let (_tmp, store) = open_test_store();
        let resp = run_request(
            store,
            r#"{"jsonrpc":"2.0","id":2,"method":"nodes_in_file","params":{"path":"nope.rs"}}"#,
        )
        .await;
        assert_eq!(resp["result"]["nodes"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error() {
        let (_tmp, store) = open_test_store();
        let resp = run_request(
            store,
            r#"{"jsonrpc":"2.0","id":9,"method":"bogus","params":{}}"#,
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn invalid_json_returns_parse_error() {
        let (_tmp, store) = open_test_store();
        let resp = run_request(store, "{not json").await;
        assert_eq!(resp["error"]["code"], -32700);
    }
}
