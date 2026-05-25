//! Daemon-side connection handler serving MCP-style RPCs out of `HotIndexes`.
//!
//! Each request is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"method":<name>,"params":{...}}`.
//! Each response is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"result":<value>}` or
//! `{"jsonrpc":"2.0","id":<n>,"error":{"code":<int>,"message":<str>}}`.
//!
//! Hot lookups hit `HotIndexes` in memory — never Cozo. Complex graph
//! analyses (transitive impact, cycles) would use `crate::query` against the
//! Cozo store; those are not exposed by this handler yet.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::daemon::ConnectionHandler;
use crate::indexes::{HotIndexes, NodeId, SymbolKey};

pub struct WorktreeHandler {
    indexes: Arc<HotIndexes>,
}

impl WorktreeHandler {
    pub fn new(indexes: Arc<HotIndexes>) -> Self {
        Self { indexes }
    }

    fn dispatch(&self, request: &Request) -> Result<Value, RpcError> {
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
                Ok(json!({ "hits": hits }))
            }
            "callers_of" => {
                let params: NodeIdParams = parse_params(request)?;
                let ids: Vec<String> = self
                    .indexes
                    .callers_of(&NodeId::from(params.node_id.as_str()))
                    .into_iter()
                    .map(|id| id.as_str().to_owned())
                    .collect();
                Ok(json!({ "node_ids": ids }))
            }
            "callees_of" => {
                let params: NodeIdParams = parse_params(request)?;
                let ids: Vec<String> = self
                    .indexes
                    .callees_of(&NodeId::from(params.node_id.as_str()))
                    .into_iter()
                    .map(|id| id.as_str().to_owned())
                    .collect();
                Ok(json!({ "node_ids": ids }))
            }
            "nodes_in_file" => {
                let params: NodesInFileParams = parse_params(request)?;
                let ids = self.indexes.nodes_in_file(&PathBuf::from(&params.path));
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
                Ok(json!({ "nodes": nodes }))
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
        let indexes = Arc::clone(&self.indexes);
        tokio::spawn(async move {
            let handler = WorktreeHandler { indexes };
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

    async fn run_request(indexes: Arc<HotIndexes>, request: &str) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handler = WorktreeHandler::new(indexes);

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
        let resp = run_request(
            Arc::new(HotIndexes::new()),
            r#"{"jsonrpc":"2.0","id":1,"method":"find_symbol","params":{"name":"Nothing"}}"#,
        )
        .await;
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["hits"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn find_symbol_returns_registered_node() {
        let indexes = Arc::new(HotIndexes::new());
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
            r#"{"jsonrpc":"2.0","id":7,"method":"find_symbol","params":{"name":"User"}}"#,
        )
        .await;
        assert_eq!(resp["id"], 7);
        let hits = resp["result"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["node_id"], "h:42");
        assert_eq!(hits[0]["qname"], "App\\Models\\User");
        assert_eq!(hits[0]["kind"], "class");
        assert_eq!(hits[0]["path"], "src/foo.rs");
    }

    #[tokio::test]
    async fn nodes_in_file_returns_registered_ids() {
        let indexes = Arc::new(HotIndexes::new());
        let path = PathBuf::from("src/foo.rs");
        indexes.insert_file(path.clone(), vec![NodeId::from("h:1"), NodeId::from("h:2")]);
        let resp = run_request(
            indexes,
            r#"{"jsonrpc":"2.0","id":3,"method":"nodes_in_file","params":{"path":"src/foo.rs"}}"#,
        )
        .await;
        let nodes = resp["result"]["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["node_id"], "h:1");
        assert_eq!(nodes[1]["node_id"], "h:2");
    }

    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error() {
        let resp = run_request(
            Arc::new(HotIndexes::new()),
            r#"{"jsonrpc":"2.0","id":9,"method":"bogus","params":{}}"#,
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);
    }
}
