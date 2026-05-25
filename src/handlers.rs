//! Daemon-side connection handler that serves a subset of MCP-style RPCs.
//!
//! Each request is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"method":<name>,"params":{...}}`.
//! Each response is one line of JSON: `{"jsonrpc":"2.0","id":<n>,"result":<value>}` or
//! `{"jsonrpc":"2.0","id":<n>,"error":{"code":<int>,"message":<str>}}`.

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
                let key = SymbolKey {
                    name: params.name,
                    kind: params.kind.unwrap_or(0),
                };
                let ids: Vec<u64> = self
                    .indexes
                    .lookup_symbol(&key)
                    .into_iter()
                    .map(|id| id.0)
                    .collect();
                Ok(json!({ "node_ids": ids }))
            }
            "callers_of" => {
                let params: NodeIdParams = parse_params(request)?;
                let ids: Vec<u64> = self
                    .indexes
                    .callers_of(NodeId(params.node_id))
                    .into_iter()
                    .map(|id| id.0)
                    .collect();
                Ok(json!({ "node_ids": ids }))
            }
            "callees_of" => {
                let params: NodeIdParams = parse_params(request)?;
                let ids: Vec<u64> = self
                    .indexes
                    .callees_of(NodeId(params.node_id))
                    .into_iter()
                    .map(|id| id.0)
                    .collect();
                Ok(json!({ "node_ids": ids }))
            }
            "nodes_in_file" => {
                let params: NodesInFileParams = parse_params(request)?;
                let ids: Vec<u64> = self
                    .indexes
                    .nodes_in_file(&PathBuf::from(params.path))
                    .into_iter()
                    .map(|id| id.0)
                    .collect();
                Ok(json!({ "node_ids": ids }))
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
    kind: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct NodeIdParams {
    node_id: u64,
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
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    async fn run_request(handler: WorktreeHandler, request: &str) -> Value {
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
    async fn find_symbol_returns_empty_for_unknown() {
        let indexes = Arc::new(HotIndexes::new());
        let handler = WorktreeHandler::new(indexes);
        let resp = run_request(
            handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"find_symbol","params":{"name":"Nothing"}}"#,
        )
        .await;
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["node_ids"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn find_symbol_returns_registered_nodes() {
        let indexes = Arc::new(HotIndexes::new());
        indexes.register_symbol(
            SymbolKey {
                name: "User".to_string(),
                kind: 0,
            },
            NodeId(42),
        );
        let handler = WorktreeHandler::new(indexes);
        let resp = run_request(
            handler,
            r#"{"jsonrpc":"2.0","id":7,"method":"find_symbol","params":{"name":"User"}}"#,
        )
        .await;
        assert_eq!(resp["id"], 7);
        let ids: Vec<u64> = serde_json::from_value(resp["result"]["node_ids"].clone()).unwrap();
        assert_eq!(ids, vec![42]);
    }

    #[tokio::test]
    async fn nodes_in_file_returns_registered_ids() {
        let indexes = Arc::new(HotIndexes::new());
        let path = PathBuf::from("src/foo.rs");
        indexes.insert_file(path.clone(), vec![NodeId(1), NodeId(2)]);
        let handler = WorktreeHandler::new(indexes);
        let resp = run_request(
            handler,
            r#"{"jsonrpc":"2.0","id":3,"method":"nodes_in_file","params":{"path":"src/foo.rs"}}"#,
        )
        .await;
        let ids: Vec<u64> = serde_json::from_value(resp["result"]["node_ids"].clone()).unwrap();
        assert_eq!(ids, vec![1, 2]);
    }

    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error() {
        let indexes = Arc::new(HotIndexes::new());
        let handler = WorktreeHandler::new(indexes);
        let resp = run_request(
            handler,
            r#"{"jsonrpc":"2.0","id":9,"method":"bogus","params":{}}"#,
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);
    }
}
