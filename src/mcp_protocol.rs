//! MCP protocol layer for the `xgraph mcp` proxy.
//!
//! LLM CLIs (Claude, Codex) speak the [Model Context Protocol] —
//! `initialize`, `tools/list`, `tools/call` — on top of JSON-RPC 2.0.
//! Our daemon answers raw JSON-RPC method calls like `find_symbol` and
//! `search`. The proxy bridges the two: handle the MCP envelope locally,
//! translate every `tools/call` into the daemon's native method, wrap
//! the response back in MCP shape.
//!
//! No code in this module talks to the socket directly — that's the
//! proxy's job. We just parse / build messages.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io/

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Protocol version we advertise. Clients negotiate via `initialize`;
/// most clients (Claude Desktop, Codex) tolerate any reasonably-recent
/// stable version. `2024-11-05` is the first stable release and is
/// universally supported.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Server identity returned during initialize. The `name` is what shows
/// up in the LLM CLI's MCP-server listing.
pub const SERVER_NAME: &str = "xgraph";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Static tool descriptor: name + human-readable description + JSON
/// schema for the input arguments. The schema mirrors the parameter
/// types the daemon's handler expects.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON schema string. Stored as a literal so we can hand it back
    /// during `tools/list` without re-parsing every time. Validated at
    /// compile time via the `#[test]` `every_tool_schema_is_valid_json`.
    pub input_schema: &'static str,
}

/// Every tool the daemon exposes, in the order they should appear to
/// the LLM. Adding a tool here makes it discoverable; the proxy then
/// forwards `tools/call` for that name to the daemon as a raw method
/// call.
pub const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "find_symbol",
        description: "Look up symbols by exact name. Optional kind filter narrows by class/function/method.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Exact symbol name to find" },
                "kind": { "type": "string", "description": "Optional symbol kind filter (class, function, method, ...)" }
            },
            "required": ["name"]
        }"#,
    },
    ToolDef {
        name: "search",
        description: "Search symbols by name with exact / prefix / contains modes plus optional kind and path-prefix filters. Backed by an in-memory trigram index — sub-millisecond at 50k symbols.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Search needle" },
                "mode": { "type": "string", "enum": ["exact", "prefix", "contains"], "description": "Match mode (default: exact)" },
                "kind": { "type": "string", "description": "Optional symbol kind filter" },
                "path_prefix": { "type": "string", "description": "Optional path-prefix filter" },
                "limit": { "type": "integer", "description": "Hard cap on hits (default 64, max 1024)" }
            },
            "required": ["name"]
        }"#,
    },
    ToolDef {
        name: "callers_of",
        description: "List node ids that call the given node id.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "node_id": { "type": "string", "description": "Target node id" }
            },
            "required": ["node_id"]
        }"#,
    },
    ToolDef {
        name: "callees_of",
        description: "List node ids called by the given node id.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "node_id": { "type": "string", "description": "Source node id" }
            },
            "required": ["node_id"]
        }"#,
    },
    ToolDef {
        name: "nodes_in_file",
        description: "List every node defined in the given file path (worktree-relative).",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Worktree-relative file path" }
            },
            "required": ["path"]
        }"#,
    },
    ToolDef {
        name: "node",
        description: "Fetch a single node by id with kind, qname, path, span, and a bounded source snippet.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "node_id": { "type": "string", "description": "Node id to fetch" }
            },
            "required": ["node_id"]
        }"#,
    },
    ToolDef {
        name: "files",
        description: "List every indexed file path (sorted).",
        input_schema: r#"{ "type": "object", "properties": {}, "additionalProperties": false }"#,
    },
    ToolDef {
        name: "status",
        description: "Graph health stats: file/node/symbol/call-edge counts, daemon RSS, pending paths, reconcile state.",
        input_schema: r#"{ "type": "object", "properties": {}, "additionalProperties": false }"#,
    },
    ToolDef {
        name: "impact",
        description: "Transitive backward closure over Calls / Inherits / Implements / References edges. 'What changes if this changes?' Optionally bounded by depth.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "node_id": { "type": "string", "description": "Target whose impact is being asked about" },
                "max_depth": { "type": "integer", "description": "Optional bound on transitive depth (0 = unbounded)" }
            },
            "required": ["node_id"]
        }"#,
    },
    ToolDef {
        name: "context",
        description: "Composite tool: find symbol + node + callers + callees in a single call. Primes task context for an agent in one round-trip.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Symbol name to look up" },
                "kind": { "type": "string", "description": "Optional kind filter" },
                "related_limit": { "type": "integer", "description": "Cap on callers/callees per direction (default 20, max 200)" },
                "snippet_bytes": { "type": "integer", "description": "Source snippet byte cap (default 2048, max 8192)" }
            },
            "required": ["name"]
        }"#,
    },
    ToolDef {
        name: "explore",
        description: "Read source snippets for a batch of node ids, partitioning a shared byte budget across them.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "node_ids": { "type": "array", "items": { "type": "string" }, "description": "Node ids to expand" },
                "byte_budget": { "type": "integer", "description": "Total byte budget across all snippets (default 32 KiB, max 128 KiB)" },
                "per_snippet_bytes": { "type": "integer", "description": "Per-snippet cap (default 4 KiB, max 16 KiB)" }
            },
            "required": ["node_ids"]
        }"#,
    },
    ToolDef {
        name: "trace",
        description: "Shortest call path between two node ids, via in-memory BFS over the call graph.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "from": { "type": "string", "description": "Source node id" },
                "to": { "type": "string", "description": "Target node id" },
                "max_depth": { "type": "integer", "description": "Hard bound on BFS depth (default 12, max 64)" }
            },
            "required": ["from", "to"]
        }"#,
    },
];

/// Build the JSON response body for `initialize`. The proxy wraps this
/// in `{"jsonrpc":"2.0","id":<n>,"result":...}` before writing it.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            // We expose tools but no resources / prompts / sampling.
            "tools": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
        }
    })
}

/// Build the JSON response body for `tools/list`.
pub fn tools_list_result() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            let schema: Value = serde_json::from_str(t.input_schema)
                .expect("tool schema must be valid JSON (checked by test)");
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// Wrap a daemon response into the MCP `tools/call` result shape. The
/// daemon returns `{"jsonrpc":"2.0","id":N,"result":<x>}` or an `error`
/// object. We surface `<x>` (or the error string) as the text content
/// of a single content block — that's the agreed-upon MCP contract
/// when a server doesn't have richer structured-content support.
pub fn wrap_tool_response(daemon_response: &Value) -> Value {
    if let Some(err) = daemon_response.get("error") {
        return json!({
            "content": [{
                "type": "text",
                "text": err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("daemon error")
            }],
            "isError": true,
        });
    }
    let result = daemon_response.get("result").cloned().unwrap_or(Value::Null);
    let text = serde_json::to_string(&result).unwrap_or_else(|_| "null".to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

/// Minimal JSON-RPC request parsing. The proxy reads one line of JSON
/// at a time; this turns it into a typed envelope so the dispatch
/// switch is readable.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    /// Notifications have no `id`. Requests must have one.
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Render a successful response payload to a single line of JSON
/// terminated by `\n`. Returning the line directly (rather than a
/// `Value`) keeps the proxy's per-message allocation count lower.
pub fn build_response_line(id: Value, result: Value) -> String {
    let mut s = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
    .expect("response is always serializable");
    s.push('\n');
    s
}

/// Same as [`build_response_line`] but for an error reply.
pub fn build_error_line(id: Value, code: i32, message: &str) -> String {
    let mut s = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
    .expect("error response is always serializable");
    s.push('\n');
    s
}

/// What kind of action should the proxy take after parsing one message
/// from stdin? Returned by [`classify_request`] so the proxy loop
/// itself stays small.
#[derive(Debug, Clone, Serialize)]
pub enum Action {
    /// We answered it ourselves; write this line to stdout.
    LocalReply(String),
    /// The original message was a notification with no response — do
    /// nothing.
    NoReply,
    /// Forward this fully-formed JSON-RPC line to the daemon, then
    /// pass the daemon's reply through to stdout. If `wrap_in_mcp` is
    /// true, the daemon's reply is wrapped in the MCP `tools/call`
    /// shape before being sent to the client.
    Forward {
        line: String,
        wrap_in_mcp: bool,
    },
    /// The message wasn't valid JSON-RPC at all. The proxy logs and
    /// drops it.
    Drop,
}

/// Decide what the proxy should do with a single incoming JSON-RPC
/// message. Pure function — easy to unit-test the MCP handshake without
/// running a real daemon.
pub fn classify_request(raw_line: &str) -> Action {
    let parsed: RpcRequest = match serde_json::from_str(raw_line.trim()) {
        Ok(p) => p,
        Err(_) => return Action::Drop,
    };
    match parsed.method.as_str() {
        // ------------------------------------------------------------
        // Locally-handled MCP envelope methods.
        // ------------------------------------------------------------
        "initialize" => {
            let id = parsed.id.unwrap_or(Value::Null);
            Action::LocalReply(build_response_line(id, initialize_result()))
        }
        "tools/list" => {
            let id = parsed.id.unwrap_or(Value::Null);
            Action::LocalReply(build_response_line(id, tools_list_result()))
        }
        "ping" => {
            // MCP defines `ping` as an empty-result request used for
            // keepalive. Some clients send it after `initialize` to
            // verify liveness.
            let id = parsed.id.unwrap_or(Value::Null);
            Action::LocalReply(build_response_line(id, json!({})))
        }
        "notifications/initialized" | "notifications/cancelled" => Action::NoReply,
        // ------------------------------------------------------------
        // Tool invocation — translate to the daemon's native method.
        // ------------------------------------------------------------
        "tools/call" => {
            let Some(name) = parsed.params.get("name").and_then(|n| n.as_str()) else {
                let id = parsed.id.unwrap_or(Value::Null);
                return Action::LocalReply(build_error_line(
                    id,
                    -32602,
                    "tools/call requires a string `name`",
                ));
            };
            if !TOOLS.iter().any(|t| t.name == name) {
                let id = parsed.id.unwrap_or(Value::Null);
                return Action::LocalReply(build_error_line(
                    id,
                    -32601,
                    &format!("unknown tool: {name}"),
                ));
            }
            let arguments = parsed
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let id = parsed.id.unwrap_or(Value::Null);
            // Forward to the daemon under the tool's native method name
            // (e.g. `tools/call` { name: "search", ... } becomes a raw
            // JSON-RPC call to `search`).
            let mut forwarded = serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": name,
                "params": arguments,
            }))
            .expect("forward payload is always serializable");
            forwarded.push('\n');
            Action::Forward {
                line: forwarded,
                wrap_in_mcp: true,
            }
        }
        // ------------------------------------------------------------
        // Anything else (legacy direct-call clients, debugging): pass
        // through unchanged.
        // ------------------------------------------------------------
        _ => {
            let mut line = raw_line.to_string();
            if !line.ends_with('\n') {
                line.push('\n');
            }
            Action::Forward {
                line,
                wrap_in_mcp: false,
            }
        }
    }
}

/// Given a daemon response line and whether we should wrap it for MCP,
/// produce the line the proxy should write to stdout.
///
/// The `incoming_id` is the id from the *client's* original message —
/// the daemon mirrors whatever id we forwarded, so for direct
/// passthrough we don't need to remap, but the tools/call path wants
/// the same id back on the wrapped reply.
pub fn shape_outgoing(daemon_line: &str, wrap_in_mcp: bool) -> String {
    if !wrap_in_mcp {
        let mut s = daemon_line.to_string();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        return s;
    }
    let daemon: Value = match serde_json::from_str(daemon_line.trim()) {
        Ok(v) => v,
        Err(_) => {
            return build_error_line(
                Value::Null,
                -32603,
                "daemon returned a malformed JSON-RPC line",
            );
        }
    };
    let id = daemon.get("id").cloned().unwrap_or(Value::Null);
    let wrapped = wrap_tool_response(&daemon);
    build_response_line(id, wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_schema_is_valid_json() {
        for tool in TOOLS {
            let _: Value = serde_json::from_str(tool.input_schema)
                .unwrap_or_else(|err| panic!("tool {} has invalid schema: {err}", tool.name));
        }
    }

    #[test]
    fn initialize_returns_capabilities_and_server_info() {
        let line = match classify_request(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        ) {
            Action::LocalReply(line) => line,
            other => panic!("expected LocalReply, got {other:?}"),
        };
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(v["result"]["capabilities"]["tools"].is_object());
        assert_eq!(v["result"]["serverInfo"]["name"], "xgraph");
    }

    #[test]
    fn tools_list_lists_every_known_tool() {
        let line = match classify_request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#) {
            Action::LocalReply(line) => line,
            other => panic!("expected LocalReply, got {other:?}"),
        };
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOLS.len());
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"find_symbol"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"trace"));
    }

    #[test]
    fn initialized_notification_yields_no_reply() {
        let action =
            classify_request(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(matches!(action, Action::NoReply));
    }

    #[test]
    fn tools_call_translates_to_native_method() {
        let action = classify_request(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"name":"User","mode":"prefix"}}}"#,
        );
        let Action::Forward { line, wrap_in_mcp } = action else {
            panic!("expected Forward");
        };
        assert!(wrap_in_mcp);
        let forwarded: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(forwarded["method"], "search");
        assert_eq!(forwarded["id"], 3);
        assert_eq!(forwarded["params"]["name"], "User");
        assert_eq!(forwarded["params"]["mode"], "prefix");
    }

    #[test]
    fn tools_call_with_unknown_tool_returns_error_locally() {
        let action = classify_request(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nonexistent","arguments":{}}}"#,
        );
        let Action::LocalReply(line) = action else {
            panic!("expected LocalReply with error");
        };
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["error"]["code"], -32601);
        assert!(v["error"]["message"].as_str().unwrap().contains("nonexistent"));
    }

    #[test]
    fn ping_returns_empty_result() {
        let action = classify_request(r#"{"jsonrpc":"2.0","id":99,"method":"ping"}"#);
        let Action::LocalReply(line) = action else {
            panic!("expected LocalReply");
        };
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 99);
        assert!(v["result"].is_object());
    }

    #[test]
    fn malformed_json_is_dropped() {
        let action = classify_request("not json at all");
        assert!(matches!(action, Action::Drop));
    }

    #[test]
    fn shape_outgoing_wraps_successful_tool_response() {
        let daemon = r#"{"jsonrpc":"2.0","id":5,"result":{"hits":[{"name":"User"}]}}"#;
        let out = shape_outgoing(daemon, true);
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["id"], 5);
        assert_eq!(v["result"]["isError"], false);
        let content = v["result"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        // The text payload contains the daemon's `result` body as JSON.
        let inner: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner["hits"][0]["name"], "User");
    }

    #[test]
    fn shape_outgoing_wraps_error_response_as_text_with_is_error() {
        let daemon = r#"{"jsonrpc":"2.0","id":6,"error":{"code":-32601,"message":"method not found"}}"#;
        let out = shape_outgoing(daemon, true);
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert!(
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("method not found")
        );
    }

    #[test]
    fn shape_outgoing_passthrough_when_not_wrapping() {
        let daemon = r#"{"jsonrpc":"2.0","id":7,"result":{"hits":[]}}"#;
        let out = shape_outgoing(daemon, false);
        assert_eq!(out.trim_end_matches('\n'), daemon);
    }
}
