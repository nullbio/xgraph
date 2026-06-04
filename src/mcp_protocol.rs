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
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "name": { "type": "string", "description": "Exact symbol name to find" },
                "kind": { "type": "string", "description": "Optional symbol kind filter (class, function, method, ...)" }
            },
            "required": ["project_root", "name"]
        }"#,
    },
    ToolDef {
        name: "search",
        description: "Search symbols by name with exact / prefix / contains modes plus optional kind and path-prefix filters. Backed by an in-memory trigram index — sub-millisecond at 50k symbols.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "name": { "type": "string", "description": "Search needle" },
                "mode": { "type": "string", "enum": ["exact", "prefix", "contains"], "description": "Match mode (default: exact)" },
                "kind": { "type": "string", "description": "Optional symbol kind filter" },
                "path_prefix": { "type": "string", "description": "Optional path-prefix filter" },
                "limit": { "type": "integer", "description": "Hard cap on hits (default 64, max 1024)" }
            },
            "required": ["project_root", "name"]
        }"#,
    },
    ToolDef {
        name: "callers_of",
        description: "List nodes that call the given node id, with metadata and pagination.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "node_id": { "type": "string", "description": "Target node id" },
                "limit": { "type": "integer", "description": "Maximum callers to return (default 200, max 1000)" },
                "offset": { "type": "integer", "description": "Number of callers to skip" }
            },
            "required": ["project_root", "node_id"]
        }"#,
    },
    ToolDef {
        name: "callees_of",
        description: "List nodes called by the given node id, with metadata and pagination.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "node_id": { "type": "string", "description": "Source node id" },
                "limit": { "type": "integer", "description": "Maximum callees to return (default 200, max 1000)" },
                "offset": { "type": "integer", "description": "Number of callees to skip" }
            },
            "required": ["project_root", "node_id"]
        }"#,
    },
    ToolDef {
        name: "nodes_in_file",
        description: "List every node defined in the given file path (worktree-relative).",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "path": { "type": "string", "description": "Worktree-relative file path" }
            },
            "required": ["project_root", "path"]
        }"#,
    },
    ToolDef {
        name: "node",
        description: "Fetch a single node by id with source, line metadata, and a bounded caller/callee trail.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "node_id": { "type": "string", "description": "Node id to fetch" },
                "related_limit": { "type": "integer", "description": "Maximum caller/callee summaries to include (default 20, max 200)" },
                "snippet_bytes": { "type": "integer", "description": "Source snippet byte cap (default 4096, max 16384)" }
            },
            "required": ["project_root", "node_id"]
        }"#,
    },
    ToolDef {
        name: "files",
        description: "List indexed file paths (sorted), with optional prefix filtering and pagination.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "prefix": { "type": "string", "description": "Optional path prefix such as app/Services" },
                "limit": { "type": "integer", "description": "Maximum paths to return" },
                "offset": { "type": "integer", "description": "Number of matching paths to skip" }
            },
            "required": ["project_root"]
        }"#,
    },
    ToolDef {
        name: "status",
        description: "Graph health stats: file/node/symbol/call-edge counts, daemon RSS, pending paths, reconcile state.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" }
            },
            "required": ["project_root"],
            "additionalProperties": false
        }"#,
    },
    ToolDef {
        name: "impact",
        description: "Transitive backward closure over Calls / Renders / Inherits / Implements / References edges. Returns affected node ids plus node metadata.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "node_id": { "type": "string", "description": "Target whose impact is being asked about" },
                "max_depth": { "type": "integer", "description": "Optional bound on transitive depth (0 = unbounded)" },
                "limit": { "type": "integer", "description": "Maximum affected nodes to return (default 500, max 5000)" },
                "offset": { "type": "integer", "description": "Number of affected nodes to skip" }
            },
            "required": ["project_root", "node_id"]
        }"#,
    },
    ToolDef {
        name: "context",
        description: "Composite tool: find symbol + node + callers + callees in a single call. Primes task context for an agent in one round-trip.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "name": { "type": "string", "description": "Symbol name to look up" },
                "mode": { "type": "string", "enum": ["exact", "prefix", "contains"], "description": "Match mode (default: exact)" },
                "kind": { "type": "string", "description": "Optional kind filter" },
                "path_prefix": { "type": "string", "description": "Optional path-prefix filter" },
                "limit": { "type": "integer", "description": "Maximum matching symbols to expand (default 20, max 200)" },
                "related_limit": { "type": "integer", "description": "Cap on callers/callees per direction (default 20, max 200)" },
                "snippet_bytes": { "type": "integer", "description": "Source snippet byte cap (default 2048, max 8192)" }
            },
            "required": ["project_root", "name"]
        }"#,
    },
    ToolDef {
        name: "explore",
        description: "Read source snippets and optional caller/callee trails for a batch of node ids, partitioning a shared byte budget across them.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "node_ids": { "type": "array", "items": { "type": "string" }, "description": "Node ids to expand" },
                "byte_budget": { "type": "integer", "description": "Total byte budget across all snippets (default 32 KiB, max 128 KiB)" },
                "per_snippet_bytes": { "type": "integer", "description": "Per-snippet cap (default 4 KiB, max 16 KiB)" },
                "related_limit": { "type": "integer", "description": "Maximum caller/callee summaries per item (default 0, max 50)" }
            },
            "required": ["project_root", "node_ids"]
        }"#,
    },
    ToolDef {
        name: "trace",
        description: "Shortest call path between two node ids, via in-memory BFS over the call graph.",
        input_schema: r#"{
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "Absolute or resolvable path inside the Git worktree to query" },
                "from": { "type": "string", "description": "Source node id" },
                "to": { "type": "string", "description": "Target node id" },
                "max_depth": { "type": "integer", "description": "Hard bound on BFS depth (default 12, max 64)" }
            },
            "required": ["project_root", "from", "to"]
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

pub fn prompts_list_result() -> Value {
    json!({ "prompts": [] })
}

pub fn resources_list_result() -> Value {
    json!({ "resources": [] })
}

pub fn resource_templates_list_result() -> Value {
    json!({ "resourceTemplates": [] })
}

/// Wrap a daemon response into the MCP `tools/call` result shape.
///
/// When `tool` is `Some`, the daemon `result` plus `meta` is rendered as
/// Markdown via [`crate::render::render_tool_result`] — that's what LLM
/// agents see. When `tool` is `None`, we fall back to serializing the raw
/// `result` as compact JSON; that path is only taken when an MCP request
/// somehow lacks tool context (defensive) and matches the legacy shape.
///
/// Errors always become MCP tool errors: the error message becomes the
/// text content and `isError` is set, mirroring the agreed-upon MCP
/// contract for servers without structured-content support.
pub fn wrap_tool_response(daemon_response: &Value, tool: Option<&ToolCall>) -> Value {
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
    let result = daemon_response
        .get("result")
        .cloned()
        .unwrap_or(Value::Null);
    let meta = daemon_response.get("meta").cloned().unwrap_or(Value::Null);
    let text = match tool {
        Some(call) => {
            crate::render::render_tool_result(&call.name, &call.arguments, &result, &meta)
        }
        None => serde_json::to_string(&result).unwrap_or_else(|_| "null".to_string()),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

/// Build a local failure response for a forwarded request that could not
/// reach the daemon. For MCP `tools/call` we keep the protocol-level request
/// successful and mark the tool result as `isError`; legacy direct callers get
/// a normal JSON-RPC error. `tool` is accepted so callers don't have to
/// re-derive it; the message itself is rendered as-is rather than through the
/// Markdown layer because forwarding errors carry no `result`/`meta` to
/// render.
pub fn shape_forward_error(
    forwarded_line: &str,
    wrap_in_mcp: bool,
    _tool: Option<&ToolCall>,
    message: &str,
) -> String {
    let parsed: Value = serde_json::from_str(forwarded_line.trim()).unwrap_or(Value::Null);
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    if wrap_in_mcp {
        return build_response_line(
            id,
            json!({
                "content": [{
                    "type": "text",
                    "text": message,
                }],
                "isError": true,
            }),
        );
    }
    build_error_line(id, -32000, message)
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

/// Identifies the MCP tool whose result the proxy is shaping. Carried
/// through [`Action::Forward`] and into [`shape_outgoing`] / [`wrap_tool_response`]
/// so the Markdown renderer can address it specifically and re-use the
/// caller's arguments (e.g. the search needle, the target node id for
/// `callers_of`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
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
    /// shape and rendered as Markdown using `tool` (which is always
    /// `Some` for `tools/call`, `None` for legacy passthrough).
    Forward {
        line: String,
        wrap_in_mcp: bool,
        tool: Option<ToolCall>,
        project_root: Option<String>,
    },
    /// The message wasn't valid JSON-RPC at all. Retained for callers
    /// that choose to ignore a message explicitly; protocol parse failures
    /// are answered with JSON-RPC errors by `classify_request`.
    Drop,
}

/// Decide what the proxy should do with a single incoming JSON-RPC
/// message. Pure function — easy to unit-test the MCP handshake without
/// running a real daemon.
pub fn classify_request(raw_line: &str) -> Action {
    let raw = raw_line.trim();
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(err) => {
            return Action::LocalReply(build_error_line(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            ));
        }
    };
    if !value.is_object() {
        return Action::LocalReply(build_error_line(
            Value::Null,
            -32600,
            "invalid request: JSON-RPC request must be an object",
        ));
    }
    if value
        .get("jsonrpc")
        .and_then(|version| version.as_str())
        .is_some_and(|version| version != "2.0")
    {
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        return Action::LocalReply(build_error_line(
            id,
            -32600,
            "invalid request: jsonrpc must be \"2.0\"",
        ));
    }
    let parsed: RpcRequest = match serde_json::from_value(value.clone()) {
        Ok(p) => p,
        Err(err) => {
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            return Action::LocalReply(build_error_line(
                id,
                -32600,
                &format!("invalid request: {err}"),
            ));
        }
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
        "prompts/list" => {
            let id = parsed.id.unwrap_or(Value::Null);
            Action::LocalReply(build_response_line(id, prompts_list_result()))
        }
        "resources/list" => {
            let id = parsed.id.unwrap_or(Value::Null);
            Action::LocalReply(build_response_line(id, resources_list_result()))
        }
        "resources/templates/list" => {
            let id = parsed.id.unwrap_or(Value::Null);
            Action::LocalReply(build_response_line(id, resource_templates_list_result()))
        }
        "ping" => {
            // MCP defines `ping` as an empty-result request used for
            // keepalive. Some clients send it after `initialize` to
            // verify liveness.
            let id = parsed.id.unwrap_or(Value::Null);
            Action::LocalReply(build_response_line(id, json!({})))
        }
        "initialized"
        | "notifications/initialized"
        | "notifications/cancelled"
        | "notifications/roots/list_changed" => Action::NoReply,
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
            let Some(project_root) = arguments
                .get("project_root")
                .and_then(|root| root.as_str())
                .map(str::to_string)
            else {
                let id = parsed.id.unwrap_or(Value::Null);
                return Action::LocalReply(build_error_line(
                    id,
                    -32602,
                    "tools/call arguments require a string `project_root`",
                ));
            };
            let mut forwarded_arguments = arguments.clone();
            if let Some(obj) = forwarded_arguments.as_object_mut() {
                obj.remove("project_root");
            }
            let id = parsed.id.unwrap_or(Value::Null);
            // Forward to the daemon under the tool's native method name
            // (e.g. `tools/call` { name: "search", ... } becomes a raw
            // JSON-RPC call to `search`).
            let mut forwarded = serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": name,
                "params": forwarded_arguments,
            }))
            .expect("forward payload is always serializable");
            forwarded.push('\n');
            Action::Forward {
                line: forwarded,
                wrap_in_mcp: true,
                tool: Some(ToolCall {
                    name: name.to_string(),
                    arguments,
                }),
                project_root: Some(project_root),
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
                tool: None,
                project_root: None,
            }
        }
    }
}

/// Given a daemon response line and whether we should wrap it for MCP,
/// produce the line the proxy should write to stdout.
///
/// The `tool` is the same value forwarded with [`Action::Forward`] — it
/// drives Markdown rendering of `result` + `meta`. Direct passthrough
/// (`wrap_in_mcp == false`) ignores it; the daemon mirrors whatever id we
/// forwarded, so the tools/call path also doesn't need to remap.
pub fn shape_outgoing(daemon_line: &str, wrap_in_mcp: bool, tool: Option<&ToolCall>) -> String {
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
    let wrapped = wrap_tool_response(&daemon, tool);
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
        let line =
            match classify_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            {
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
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"find_symbol"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"trace"));
    }

    #[test]
    fn unsupported_capability_lists_are_empty_local_replies() {
        for (method, result_key) in [
            ("prompts/list", "prompts"),
            ("resources/list", "resources"),
            ("resources/templates/list", "resourceTemplates"),
        ] {
            let action = classify_request(&format!(
                r#"{{"jsonrpc":"2.0","id":10,"method":"{method}"}}"#
            ));
            let Action::LocalReply(line) = action else {
                panic!("expected LocalReply for {method}");
            };
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(v["id"], 10);
            assert_eq!(v["result"][result_key].as_array().unwrap().len(), 0);
        }
    }

    #[test]
    fn initialized_notification_yields_no_reply() {
        let action = classify_request(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(matches!(action, Action::NoReply));
        let action = classify_request(r#"{"jsonrpc":"2.0","method":"initialized"}"#);
        assert!(matches!(action, Action::NoReply));
    }

    #[test]
    fn tools_call_translates_to_native_method() {
        let action = classify_request(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"project_root":"/tmp/project-a","name":"User","mode":"prefix"}}}"#,
        );
        let Action::Forward {
            line,
            wrap_in_mcp,
            tool,
            project_root,
        } = action
        else {
            panic!("expected Forward");
        };
        assert!(wrap_in_mcp);
        assert_eq!(project_root.as_deref(), Some("/tmp/project-a"));
        let tool = tool.expect("tools/call should carry tool context");
        assert_eq!(tool.name, "search");
        assert_eq!(tool.arguments["project_root"], "/tmp/project-a");
        assert_eq!(tool.arguments["name"], "User");
        assert_eq!(tool.arguments["mode"], "prefix");
        let forwarded: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(forwarded["method"], "search");
        assert_eq!(forwarded["id"], 3);
        assert!(forwarded["params"].get("project_root").is_none());
        assert_eq!(forwarded["params"]["name"], "User");
        assert_eq!(forwarded["params"]["mode"], "prefix");
    }

    #[test]
    fn tools_call_requires_project_root() {
        let action = classify_request(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"name":"User","mode":"prefix"}}}"#,
        );
        let Action::LocalReply(line) = action else {
            panic!("expected LocalReply error");
        };
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32602);
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("project_root")
        );
    }

    #[test]
    fn legacy_passthrough_carries_no_tool_context() {
        let action = classify_request(r#"{"jsonrpc":"2.0","id":1,"method":"search","params":{}}"#);
        let Action::Forward {
            tool,
            wrap_in_mcp,
            project_root,
            ..
        } = action
        else {
            panic!("expected Forward");
        };
        assert!(!wrap_in_mcp);
        assert!(tool.is_none());
        assert!(project_root.is_none());
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
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("nonexistent")
        );
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
    fn malformed_json_returns_parse_error() {
        let action = classify_request("not json at all");
        let Action::LocalReply(line) = action else {
            panic!("expected LocalReply parse error");
        };
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], Value::Null);
        assert_eq!(v["error"]["code"], -32700);
    }

    #[test]
    fn invalid_jsonrpc_version_returns_invalid_request() {
        let action = classify_request(r#"{"jsonrpc":"1.0","id":55,"method":"initialize"}"#);
        let Action::LocalReply(line) = action else {
            panic!("expected LocalReply invalid request");
        };
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 55);
        assert_eq!(v["error"]["code"], -32600);
    }

    fn search_tool_call() -> ToolCall {
        ToolCall {
            name: "search".to_string(),
            arguments: json!({"project_root": "/tmp/project-a", "name": "User", "mode": "exact"}),
        }
    }

    #[test]
    fn shape_outgoing_renders_tool_response_as_markdown() {
        let daemon = r#"{"jsonrpc":"2.0","id":5,"result":{"hits":[{"node_id":"abc:1","name":"User","qname":"App\\Models\\User","kind":"class","path":"app/Models/User.php"}]},"meta":{"catching_up":false,"rss_bytes":1048576,"pending_paths":0,"warnings":[]}}"#;
        let tool = search_tool_call();
        let out = shape_outgoing(daemon, true, Some(&tool));
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["id"], 5);
        assert_eq!(v["result"]["isError"], false);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("xgraph project: /tmp/project-a"));
        assert!(text.contains("## Search: User"));
        assert!(text.contains("App\\Models\\User"));
        assert!(text.contains("abc:1"));
        assert!(text.contains("Index State: current."));
    }

    #[test]
    fn shape_outgoing_without_tool_falls_back_to_json_text() {
        let daemon = r#"{"jsonrpc":"2.0","id":5,"result":{"hits":[{"name":"User"}]}}"#;
        let out = shape_outgoing(daemon, true, None);
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["result"]["isError"], false);
        let inner: Value =
            serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner["hits"][0]["name"], "User");
    }

    #[test]
    fn shape_outgoing_wraps_error_response_as_text_with_is_error() {
        let daemon =
            r#"{"jsonrpc":"2.0","id":6,"error":{"code":-32601,"message":"method not found"}}"#;
        let tool = search_tool_call();
        let out = shape_outgoing(daemon, true, Some(&tool));
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
        let out = shape_outgoing(daemon, false, None);
        assert_eq!(out.trim_end_matches('\n'), daemon);
    }

    #[test]
    fn shape_forward_error_uses_mcp_tool_error_shape() {
        let forwarded = r#"{"jsonrpc":"2.0","id":8,"method":"search","params":{}}"#;
        let tool = search_tool_call();
        let out = shape_forward_error(forwarded, true, Some(&tool), "daemon unavailable");
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["id"], 8);
        assert_eq!(v["result"]["isError"], true);
        assert_eq!(v["result"]["content"][0]["text"], "daemon unavailable");
    }

    #[test]
    fn shape_forward_error_uses_jsonrpc_error_for_direct_calls() {
        let forwarded = r#"{"jsonrpc":"2.0","id":9,"method":"search","params":{}}"#;
        let out = shape_forward_error(forwarded, false, None, "daemon unavailable");
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32000);
    }
}
