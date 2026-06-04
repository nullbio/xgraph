//! Markdown rendering of daemon tool responses for the MCP boundary.
//!
//! The daemon returns compact JSON; LLM agents read Markdown far better.
//! This module is invoked only by [`crate::mcp_protocol::wrap_tool_response`]
//! when an MCP `tools/call` is being wrapped — direct JSON-RPC callers still
//! see the raw daemon shape.
//!
//! The rendered Markdown is lossless relative to the JSON: every field in
//! `result` and `meta` either appears inline, in a table, or in a short-ref
//! + full-ID appendix. Empty-result branches include a hint on what to try next.

use std::collections::BTreeSet;
use std::fmt::Write;

use serde_json::Value;

const IMPACT_EDGE_KINDS: &str = "calls, renders, inherits, implements, references";

/// Render the daemon result for a single MCP tool call.
///
/// `args` is the original `arguments` object the client sent in `tools/call`;
/// it carries the query/filters that don't appear in the daemon's reply
/// (e.g. the search needle, the requested file path, the target node id for
/// `callers_of`/`callees_of`).
pub fn render_tool_result(tool: &str, args: &Value, result: &Value, meta: &Value) -> String {
    let body = match tool {
        "status" => render_status(result),
        "files" => render_files(args, result),
        "search" => render_search(args, result),
        "find_symbol" => render_find_symbol(args, result),
        "nodes_in_file" => render_nodes_in_file(args, result),
        "node" => render_node(result),
        "callers_of" => render_related_nodes("Callers", "Caller", args, result),
        "callees_of" => render_related_nodes("Callees", "Callee", args, result),
        "impact" => render_impact(args, result),
        "context" => render_context(args, result),
        "explore" => render_explore(args, result),
        "trace" => render_trace(args, result),
        "sync" | "reindex" => render_maintenance(result),
        other => render_unknown(other, result),
    };
    let body = match args["project_root"]
        .as_str()
        .filter(|root| !root.is_empty())
    {
        Some(root) => format!("xgraph project: {root}\n\n{body}"),
        None => body,
    };
    let footer = render_index_state(meta);
    if footer.is_empty() {
        body
    } else {
        let mut out = body;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&footer);
        out
    }
}

// ---------------------------------------------------------------------------
// Per-tool renderers
// ---------------------------------------------------------------------------

fn render_status(result: &Value) -> String {
    let files = result["files"].as_u64().unwrap_or(0);
    let nodes = result["nodes"].as_u64().unwrap_or(0);
    let symbols = result["symbols"].as_u64().unwrap_or(0);
    let call_edges = result["call_edges"].as_u64().unwrap_or(0);
    let rss = result["rss_bytes"].as_u64().unwrap_or(0);
    let pending = result["pending_paths"].as_u64().unwrap_or(0);
    let done = result["reconcile_done"].as_bool().unwrap_or(false);

    let mut out = String::from("## xgraph Status\n\n");
    let ready = if done { "yes" } else { "no" };
    let _ = writeln!(out, "Ready: {ready}. Pending paths: {pending}.");
    out.push('\n');

    let table = render_table(
        &["Files", "Nodes", "Symbols", "Call edges", "RSS"],
        &[vec![
            files.to_string(),
            nodes.to_string(),
            symbols.to_string(),
            call_edges.to_string(),
            format_mib(rss),
        ]],
        &[Align::Right; 5],
    );
    out.push_str(&table);
    out
}

fn render_files(args: &Value, result: &Value) -> String {
    let prefix = args["prefix"].as_str().unwrap_or("");
    let files = result["files"].as_array().cloned().unwrap_or_default();
    let total = result["total"].as_u64().unwrap_or(0) as usize;
    let offset = result["offset"].as_u64().unwrap_or(0) as usize;
    let limit = result["limit"].as_u64().unwrap_or(0) as usize;

    let mut out = String::new();
    if prefix.is_empty() {
        out.push_str("## Files\n");
    } else {
        let _ = writeln!(out, "## Files: {prefix}");
    }

    if total == 0 {
        out.push('\n');
        match prefix {
            "" => out.push_str("No files indexed. Try `status` to confirm reconcile state.\n"),
            _ => {
                let _ = writeln!(
                    out,
                    "No files match prefix `{prefix}`. Drop or shorten the prefix to widen the search."
                );
            }
        }
        return out;
    }

    let returned = files.len();
    let truncated = offset.saturating_add(returned) < total;
    let _ = writeln!(
        out,
        "{}",
        pagination_line(offset, returned, total, truncated)
    );
    let _ = writeln!(
        out,
        "Page: offset {offset}, limit {limit} (returned {returned}, total {total}, truncated {truncated})."
    );
    out.push('\n');
    for f in &files {
        if let Some(s) = f.as_str() {
            let _ = writeln!(out, "- {s}");
        }
    }
    out
}

fn render_search(args: &Value, result: &Value) -> String {
    let needle = args["name"].as_str().unwrap_or("");
    let mode = args["mode"].as_str().unwrap_or("exact");
    let kind_filter = args["kind"].as_str().filter(|s| !s.is_empty());
    let path_filter = args["path_prefix"].as_str().filter(|s| !s.is_empty());
    let limit_arg = args["limit"].as_u64();
    let hits = result["hits"].as_array().cloned().unwrap_or_default();
    let n = hits.len();

    let mut out = String::new();
    let _ = writeln!(out, "## Search: {needle}");
    out.push('\n');

    let hit_word = if n == 1 { "hit" } else { "hits" };
    let mut summary = format!("{n} {hit_word}. Mode: {mode}.");
    if let Some(k) = kind_filter {
        let _ = write!(summary, " Kind: {k}.");
    }
    if let Some(p) = path_filter {
        let _ = write!(summary, " Path prefix: {p}.");
    }
    if let Some(l) = limit_arg {
        let _ = write!(summary, " Limit: {l}.");
    }
    let _ = writeln!(out, "{summary}");

    if n == 0 {
        let _ = writeln!(
            out,
            "\nNo hits. Try `search` with `mode=contains` (or `prefix`), drop the kind/path filter, or run `find_symbol` if you know the exact name."
        );
        return out;
    }

    out.push('\n');
    out.push_str(&render_node_table(&hits, "Symbol"));
    out
}

fn render_find_symbol(args: &Value, result: &Value) -> String {
    let needle = args["name"].as_str().unwrap_or("");
    let kind_filter = args["kind"].as_str().filter(|s| !s.is_empty());
    let hits = result["hits"].as_array().cloned().unwrap_or_default();
    let n = hits.len();

    let mut out = String::new();
    let _ = writeln!(out, "## Symbol Lookup: {needle}");
    out.push('\n');

    if n == 0 {
        let _ = writeln!(
            out,
            "No exact symbol found. Try `search` with `mode=contains` or `mode=prefix`, or drop the `kind` filter."
        );
        return out;
    }

    let mut summary = format!("Exact matches: {n}.");
    if let Some(k) = kind_filter {
        let _ = write!(summary, " Kind: {k}.");
    }
    let _ = writeln!(out, "{summary}");
    out.push('\n');
    out.push_str(&render_node_table(&hits, "Symbol"));
    out
}

fn render_nodes_in_file(args: &Value, result: &Value) -> String {
    let path = args["path"].as_str().unwrap_or("");
    let nodes = result["nodes"].as_array().cloned().unwrap_or_default();

    let mut out = String::new();
    out.push_str("## Nodes In File\n");
    let _ = writeln!(out, "{path}");
    out.push('\n');

    if nodes.is_empty() {
        let _ = writeln!(
            out,
            "No nodes indexed for `{path}`. Check the path is worktree-relative, or run `status` to see whether the file is still pending."
        );
        return out;
    }

    // Group by kind, preserve source order within each group.
    let mut by_kind: Vec<(String, Vec<&Value>)> = Vec::new();
    for n in &nodes {
        let k = n["kind"].as_str().unwrap_or("unknown").to_string();
        if let Some(group) = by_kind.iter_mut().find(|(kind, _)| kind == &k) {
            group.1.push(n);
        } else {
            by_kind.push((k, vec![n]));
        }
    }

    let total = nodes.len();
    let summary = by_kind
        .iter()
        .map(|(k, v)| format!("{} {}", v.len(), pluralize(k, v.len())))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(out, "{total} nodes: {summary}.");

    for (kind, items) in &by_kind {
        let header = capitalize(&pluralize(kind, items.len()));
        let _ = writeln!(out, "\n{header}:");
        for n in items {
            let display = display_name(n);
            let id = n["node_id"].as_str().unwrap_or("");
            let _ = writeln!(out, "- {display} — {id}");
        }
    }
    out
}

fn render_node(result: &Value) -> String {
    let node = &result["node"];
    if node.is_null() {
        return "## Node\n\nNode not found. Verify the `node_id` via `search` or `find_symbol`.\n"
            .to_string();
    }

    let qname = node["qname"].as_str().unwrap_or("");
    let name = node["name"].as_str().unwrap_or("");
    let kind = node["kind"].as_str().unwrap_or("");
    let path = node["path"].as_str().unwrap_or("");
    let id = node["node_id"].as_str().unwrap_or("");
    let display = if !qname.is_empty() { qname } else { name };

    let mut out = String::new();
    let _ = writeln!(out, "## {display}");
    let location = format_location(path, &node["span"]);
    let _ = writeln!(out, "{kind} at {location}");
    let _ = writeln!(out, "Node ID: {id}");
    if let Some(span_line) = format_span_detail(&node["span"]) {
        let _ = writeln!(out, "{span_line}");
    }

    let mut ids = IdAppendix::new();

    let callee_nodes = node["callee_nodes"].as_array().cloned().unwrap_or_default();
    render_bullet_section(&mut out, "Calls", &callee_nodes, &mut ids);

    let caller_nodes = node["caller_nodes"].as_array().cloned().unwrap_or_default();
    render_bullet_section(&mut out, "Called by", &caller_nodes, &mut ids);

    let members = node["member_nodes"].as_array().cloned().unwrap_or_default();
    render_member_section(&mut out, "Members", &members, &mut ids);

    let member_callers = node["member_caller_nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    render_bullet_section(&mut out, "Member callers", &member_callers, &mut ids);

    let member_callees = node["member_callee_nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    render_bullet_section(&mut out, "Member callees", &member_callees, &mut ids);

    if let Some(source_block) = format_source_block(path, node) {
        out.push('\n');
        out.push_str(&source_block);
    }

    out.push_str(&ids.render());
    out
}

fn render_related_nodes(title: &str, col_title: &str, args: &Value, result: &Value) -> String {
    let target_id = args["node_id"].as_str().unwrap_or("");
    let nodes = result["nodes"].as_array().cloned().unwrap_or_default();
    let total = result["total"].as_u64().unwrap_or(0) as usize;
    let offset = result["offset"].as_u64().unwrap_or(0) as usize;
    let limit = result["limit"].as_u64().unwrap_or(0) as usize;
    let truncated = result["truncated"].as_bool().unwrap_or(false);

    let mut out = String::new();
    let _ = writeln!(out, "## {title} Of {target_id}");
    out.push('\n');

    let word_lower = col_title.to_lowercase();
    let word = pluralize(&word_lower, total);
    let _ = writeln!(out, "{total} {word}.");

    if nodes.is_empty() {
        let _ = writeln!(
            out,
            "No {word} recorded. Confirm the node exists with `node node_id={target_id}`, or check `status` for indexing progress."
        );
        return out;
    }

    let returned = nodes.len();
    let _ = writeln!(
        out,
        "{}",
        pagination_line(offset, returned, total, truncated)
    );
    let _ = writeln!(
        out,
        "Page: offset {offset}, limit {limit} (returned {returned}, total {total}, truncated {truncated})."
    );

    out.push('\n');
    out.push_str(&render_node_table(&nodes, col_title));
    out
}

fn render_impact(args: &Value, result: &Value) -> String {
    let target = &result["target"];
    let target_qname = target["qname"].as_str().unwrap_or("");
    let target_id = target["node_id"]
        .as_str()
        .or(args["node_id"].as_str())
        .unwrap_or("");
    let target_label = if target_qname.is_empty() {
        target_id.to_string()
    } else {
        target_qname.to_string()
    };
    let target_path = target["path"].as_str().unwrap_or("");

    let nodes = result["nodes"].as_array().cloned().unwrap_or_default();
    let total = result["total"].as_u64().unwrap_or(0) as usize;
    let offset = result["offset"].as_u64().unwrap_or(0) as usize;
    let limit = result["limit"].as_u64().unwrap_or(0) as usize;
    let truncated = result["truncated"].as_bool().unwrap_or(false);
    let max_depth = args["max_depth"].as_u64().unwrap_or(0);
    let depth_label = if max_depth == 0 {
        "unbounded".to_string()
    } else {
        max_depth.to_string()
    };

    let mut out = String::new();
    let _ = writeln!(out, "## Impact: {target_label}");
    if !target_path.is_empty() {
        let _ = writeln!(out, "Target: {target_id} ({target_path})");
    } else {
        let _ = writeln!(out, "Target: {target_id}");
    }
    let node_word = if total == 1 { "node" } else { "nodes" };
    let _ = writeln!(
        out,
        "{total} affected {node_word}. Depth: {depth_label}. Edges: {IMPACT_EDGE_KINDS}."
    );
    let returned = nodes.len();
    if total > 0 {
        let _ = writeln!(
            out,
            "{}",
            pagination_line(offset, returned, total, truncated)
        );
        let _ = writeln!(
            out,
            "Page: offset {offset}, limit {limit} (returned {returned}, total {total}, truncated {truncated})."
        );
    }

    if nodes.is_empty() {
        if total == 0 {
            let _ = writeln!(
                out,
                "\nNothing in the index depends on this node. Confirm the target with `node node_id={target_id}`."
            );
        }
        return out;
    }

    // Group by path, preserve order.
    let mut by_path: Vec<(String, Vec<&Value>)> = Vec::new();
    for n in &nodes {
        let p = n["path"].as_str().unwrap_or("").to_string();
        if let Some(group) = by_path.iter_mut().find(|(path, _)| path == &p) {
            group.1.push(n);
        } else {
            by_path.push((p, vec![n]));
        }
    }

    for (path, items) in &by_path {
        let label = if path.is_empty() {
            "(unknown path)"
        } else {
            path
        };
        let _ = writeln!(out, "\n{label}");
        for n in items {
            let display = display_name(n);
            let kind = n["kind"].as_str().unwrap_or("");
            let id = n["node_id"].as_str().unwrap_or("");
            let _ = writeln!(out, "- {display} — {kind} — {id}");
        }
    }
    out
}

fn render_context(args: &Value, result: &Value) -> String {
    let query = args["name"].as_str().unwrap_or("");
    let mode = args["mode"].as_str().unwrap_or("exact");
    let kind_filter = args["kind"].as_str().filter(|s| !s.is_empty());
    let path_filter = args["path_prefix"].as_str().filter(|s| !s.is_empty());
    let matches = result["matches"].as_array().cloned().unwrap_or_default();
    let total = result["total_matches"].as_u64().unwrap_or(0) as usize;
    let limit = result["limit"].as_u64().unwrap_or(0) as usize;

    let mut out = String::from("## Code Context\n");
    let _ = writeln!(out, "Query: {query}");
    let _ = writeln!(out, "Mode: {mode}");
    if let Some(k) = kind_filter {
        let _ = writeln!(out, "Kind filter: {k}");
    }
    if let Some(p) = path_filter {
        let _ = writeln!(out, "Path prefix: {p}");
    }
    out.push('\n');

    if matches.is_empty() {
        let _ = writeln!(
            out,
            "No matches for `{query}`. Try `context` with `mode=contains`, drop filters, or check `status` for pending paths."
        );
        return out;
    }

    let returned = matches.len();
    if total > returned {
        let _ = writeln!(
            out,
            "{total} total matches; expanded first {returned} (limit {limit})."
        );
    } else {
        let _ = writeln!(
            out,
            "{returned} of {total} matches expanded (limit {limit})."
        );
    }

    let mut ids = IdAppendix::new();

    out.push_str("\nEntry Points:\n");
    for m in &matches {
        let qname = m["qname"].as_str().unwrap_or("");
        let kind = m["kind"].as_str().unwrap_or("");
        let path = m["path"].as_str().unwrap_or("");
        let id = m["node_id"].as_str().unwrap_or("");
        let location = format_location(path, &m["span"]);
        let _ = writeln!(out, "- {qname} — {kind} — {location} — {id}");
        ids.add(qname, id);
    }

    for m in &matches {
        let qname = m["qname"].as_str().unwrap_or("");
        let path = m["path"].as_str().unwrap_or("");
        let id = m["node_id"].as_str().unwrap_or("");
        let members = m["member_nodes"].as_array().cloned().unwrap_or_default();
        let callers = m["caller_nodes"].as_array().cloned().unwrap_or_default();
        let callees = m["callee_nodes"].as_array().cloned().unwrap_or_default();
        let member_callers = m["member_caller_nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let member_callees = m["member_callee_nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        if !members.is_empty() {
            let _ = writeln!(out, "\nRelated Members of {qname}:");
            for member in &members {
                let mn = display_name(member);
                let mk = member["kind"].as_str().unwrap_or("");
                let mid = member["node_id"].as_str().unwrap_or("");
                let _ = writeln!(out, "- {mn} — {mk}");
                ids.add(mn, mid);
            }
        }

        if !callers.is_empty() || !callees.is_empty() {
            let _ = writeln!(out, "\nRelationships for {qname}:");
            for cer in &callers {
                let from = display_name(cer);
                let from_id = cer["node_id"].as_str().unwrap_or("");
                let _ = writeln!(out, "- {from} → {qname}");
                ids.add(from, from_id);
            }
            for cee in &callees {
                let to = display_name(cee);
                let to_id = cee["node_id"].as_str().unwrap_or("");
                let _ = writeln!(out, "- {qname} → {to}");
                ids.add(to, to_id);
            }
        }

        if !member_callers.is_empty() {
            let _ = writeln!(out, "\nMember Callers of {qname}:");
            for c in &member_callers {
                let from = display_name(c);
                let fid = c["node_id"].as_str().unwrap_or("");
                let _ = writeln!(out, "- {from}");
                ids.add(from, fid);
            }
        }
        if !member_callees.is_empty() {
            let _ = writeln!(out, "\nMember Callees of {qname}:");
            for c in &member_callees {
                let to = display_name(c);
                let tid = c["node_id"].as_str().unwrap_or("");
                let _ = writeln!(out, "- {to}");
                ids.add(to, tid);
            }
        }

        if let Some(source_block) = format_source_block(path, m) {
            let _ = writeln!(out, "\n### {qname}");
            out.push_str(&source_block);
        }
        ids.add(qname, id);
    }

    out.push_str(&ids.render());
    out
}

fn render_explore(args: &Value, result: &Value) -> String {
    let requested_ids = args["node_ids"].as_array().cloned().unwrap_or_default();
    let items = result["items"].as_array().cloned().unwrap_or_default();
    let bytes_used = result["bytes_used"].as_u64().unwrap_or(0);
    let bytes_budget = result["bytes_budget"].as_u64().unwrap_or(0);

    let mut out = String::from("## Exploration\n");

    // Count unique files across items.
    let mut file_set: BTreeSet<String> = BTreeSet::new();
    for it in &items {
        if let Some(p) = it["path"].as_str() {
            file_set.insert(p.to_string());
        }
    }
    let n = items.len();
    let nf = file_set.len();
    let _ = writeln!(
        out,
        "{n} {} across {nf} {}. Used {} / {} bytes.",
        if n == 1 { "node" } else { "nodes" },
        if nf == 1 { "file" } else { "files" },
        format_thousands(bytes_used),
        format_thousands(bytes_budget),
    );
    let _ = writeln!(out, "Requested ids: {}", requested_ids.len());

    let missing: Vec<&Value> = items.iter().filter(|it| it["node"].is_null()).collect();
    if !missing.is_empty() {
        out.push_str("\nMissing ids (not in index):\n");
        for it in &missing {
            if let Some(s) = it["node_id"].as_str() {
                let _ = writeln!(out, "- {s}");
            }
        }
    }

    if items.is_empty() {
        let _ = writeln!(
            out,
            "\nNo items returned. Verify ids via `node` or `search` before retrying."
        );
        return out;
    }

    let mut ids = IdAppendix::new();

    // Relationships pulled from per-item caller/callee summaries.
    let mut rels: Vec<String> = Vec::new();
    for it in &items {
        let qname = display_name(it);
        if qname.is_empty() {
            continue;
        }
        for c in it["caller_nodes"].as_array().cloned().unwrap_or_default() {
            let from = display_name(&c);
            if !from.is_empty() {
                rels.push(format!("- {from} → {qname}"));
                ids.add(from, c["node_id"].as_str().unwrap_or(""));
            }
        }
        for c in it["callee_nodes"].as_array().cloned().unwrap_or_default() {
            let to = display_name(&c);
            if !to.is_empty() {
                rels.push(format!("- {qname} → {to}"));
                ids.add(to, c["node_id"].as_str().unwrap_or(""));
            }
        }
    }
    if !rels.is_empty() {
        out.push_str("\nRelationships:\n");
        for r in &rels {
            let _ = writeln!(out, "{r}");
        }
    }

    // Group by file, preserve item order.
    let mut by_path: Vec<(String, Vec<&Value>)> = Vec::new();
    for it in &items {
        let p = it["path"].as_str().unwrap_or("").to_string();
        if let Some(g) = by_path.iter_mut().find(|(path, _)| path == &p) {
            g.1.push(it);
        } else {
            by_path.push((p, vec![it]));
        }
    }

    for (path, group) in &by_path {
        if path.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n### {path}");
        for it in group {
            let qname = display_name(it);
            let id = it["node_id"].as_str().unwrap_or("");
            let location = format_location(path, &it["span"]);
            let _ = writeln!(out, "#### {qname} — {id}");
            let _ = writeln!(out, "{location}");
            if let Some(block) = format_source_block(path, it) {
                out.push_str(&block);
            }
            ids.add(qname, id);
        }
    }

    out.push_str(&ids.render());
    out
}

fn render_trace(args: &Value, result: &Value) -> String {
    let from_id = args["from"].as_str().unwrap_or("");
    let to_id = args["to"].as_str().unwrap_or("");
    let max_depth = args["max_depth"].as_u64();

    let path = result["path"].as_array().cloned();
    let length = result["length"].as_u64();

    let mut out = String::new();

    let Some(hops) = path else {
        let _ = writeln!(out, "## Trace: {from_id} → {to_id}");
        let depth_note = match max_depth {
            Some(d) => format!(" within max_depth={d}"),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "No path found{depth_note}. Try increasing `max_depth`, or run `callers_of`/`callees_of` on either endpoint to check connectivity."
        );
        return out;
    };

    if hops.is_empty() {
        let _ = writeln!(out, "## Trace: {from_id} → {to_id}");
        let _ = writeln!(out, "Empty path.");
        return out;
    }

    let first = &hops[0];
    let last = &hops[hops.len() - 1];
    let from_label = display_name(first);
    let to_label = display_name(last);
    let from_label = if from_label.is_empty() {
        from_id
    } else {
        from_label
    };
    let to_label = if to_label.is_empty() { to_id } else { to_label };

    let _ = writeln!(out, "## Trace: {from_label} → {to_label}");
    let hop_count = length.unwrap_or((hops.len() as u64).saturating_sub(1));
    let hop_word = if hop_count == 1 { "hop" } else { "hops" };
    let _ = writeln!(out, "Shortest path: {hop_count} {hop_word}.");
    out.push('\n');

    for (i, hop) in hops.iter().enumerate() {
        let qname = display_name(hop);
        let kind = hop["kind"].as_str().unwrap_or("");
        let path = hop["path"].as_str().unwrap_or("");
        let id = hop["node_id"].as_str().unwrap_or("");
        let _ = writeln!(out, "{}. {qname}", i + 1);
        if !kind.is_empty() && !path.is_empty() {
            let _ = writeln!(out, "   {kind} at {path}");
        } else if !path.is_empty() {
            let _ = writeln!(out, "   {path}");
        }
        let _ = writeln!(out, "   Node ID: {id}");
        if i + 1 < hops.len() {
            let next = display_name(&hops[i + 1]);
            let _ = writeln!(out, "   calls → {next}");
        }
        out.push('\n');
    }

    let ids: Vec<String> = hops
        .iter()
        .filter_map(|n| n["node_id"].as_str().map(String::from))
        .collect();
    let _ = writeln!(
        out,
        "Callsite line numbers are not yet stored. Use `explore` with these node IDs for source context: {}.",
        ids.join(", ")
    );

    out
}

fn render_maintenance(result: &Value) -> String {
    let op = result["operation"].as_str().unwrap_or("maintenance");
    let mut out = String::new();
    let _ = writeln!(out, "## {} complete", capitalize(op));
    let _ = writeln!(
        out,
        "Files scanned: {}. Indexed: {}. Nodes created: {}. Edges created: {}.",
        result["files_scanned"].as_u64().unwrap_or(0),
        result["files_indexed"].as_u64().unwrap_or(0),
        result["nodes_created"].as_u64().unwrap_or(0),
        result["edges_created"].as_u64().unwrap_or(0),
    );
    let graph = &result["graph"];
    let _ = writeln!(
        out,
        "Graph: {} files, {} nodes, {} symbols, {} call edges.",
        graph["files"].as_u64().unwrap_or(0),
        graph["nodes"].as_u64().unwrap_or(0),
        graph["symbols"].as_u64().unwrap_or(0),
        graph["call_edges"].as_u64().unwrap_or(0),
    );
    let t = &result["timings"];
    let _ = writeln!(
        out,
        "Timings (µs): scan {}, parse {}, resolve {}, store {}.",
        t["scan_us"].as_u64().unwrap_or(0),
        t["parse_us"].as_u64().unwrap_or(0),
        t["resolve_us"].as_u64().unwrap_or(0),
        t["store_us"].as_u64().unwrap_or(0),
    );
    out
}

fn render_unknown(tool: &str, result: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## {tool}");
    out.push_str("```json\n");
    let pretty = serde_json::to_string_pretty(result).unwrap_or_else(|_| "null".into());
    out.push_str(&pretty);
    if !pretty.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

fn render_index_state(meta: &Value) -> String {
    if meta.is_null() {
        return String::new();
    }
    let catching_up = meta["catching_up"].as_bool().unwrap_or(false);
    let pending = meta["pending_paths"].as_u64().unwrap_or(0);
    let rss = meta["rss_bytes"].as_u64().unwrap_or(0);
    let warnings = meta["warnings"].as_array().cloned().unwrap_or_default();

    let mut out = String::new();
    if catching_up {
        let path_word = if pending == 1 {
            "pending path"
        } else {
            "pending paths"
        };
        let _ = writeln!(
            out,
            "Index State: catching up. Results may be stale for {pending} {path_word}. RSS: {}.",
            format_mib(rss)
        );
    } else {
        let _ = writeln!(
            out,
            "Index State: current. Pending paths: {pending}. RSS: {}.",
            format_mib(rss)
        );
    }
    for w in &warnings {
        let kind = w["kind"].as_str().unwrap_or("");
        if kind == "high_memory_usage" {
            let rss = w["rss_bytes"].as_u64().unwrap_or(0);
            let threshold = w["threshold_bytes"].as_u64().unwrap_or(0);
            let _ = writeln!(
                out,
                "Warning: high memory usage. RSS {} exceeds {}.",
                format_mib(rss),
                format_mib(threshold)
            );
        } else if !kind.is_empty() {
            let _ = writeln!(out, "Warning: {kind}.");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn render_node_table(nodes: &[Value], primary_col: &str) -> String {
    let rows: Vec<Vec<String>> = nodes
        .iter()
        .map(|n| {
            let qname = n["qname"].as_str().unwrap_or("");
            let name = n["name"].as_str().unwrap_or("");
            let symbol = if qname.is_empty() { name } else { qname };
            vec![
                symbol.to_string(),
                n["kind"].as_str().unwrap_or("").to_string(),
                n["path"].as_str().unwrap_or("").to_string(),
                n["node_id"].as_str().unwrap_or("").to_string(),
            ]
        })
        .collect();
    render_table(
        &[primary_col, "Kind", "File", "Node ID"],
        &rows,
        &[Align::Left; 4],
    )
}

fn render_bullet_section(out: &mut String, header: &str, nodes: &[Value], ids: &mut IdAppendix) {
    if nodes.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n{header}:");
    for n in nodes {
        let label = display_name(n);
        let id = n["node_id"].as_str().unwrap_or("");
        let _ = writeln!(out, "- {label}");
        ids.add(label, id);
    }
}

fn render_member_section(out: &mut String, header: &str, nodes: &[Value], ids: &mut IdAppendix) {
    if nodes.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n{header}:");
    for n in nodes {
        let label = display_name(n);
        let kind = n["kind"].as_str().unwrap_or("");
        let id = n["node_id"].as_str().unwrap_or("");
        if kind.is_empty() {
            let _ = writeln!(out, "- {label}");
        } else {
            let _ = writeln!(out, "- {label} — {kind}");
        }
        ids.add(label, id);
    }
}

fn display_name(n: &Value) -> &str {
    let qname = n["qname"].as_str().unwrap_or("");
    if !qname.is_empty() {
        return qname;
    }
    let name = n["name"].as_str().unwrap_or("");
    if !name.is_empty() {
        return name;
    }
    n["node_id"].as_str().unwrap_or("")
}

fn format_location(path: &str, span: &Value) -> String {
    let line = span["start_line"].as_u64();
    match (path.is_empty(), line) {
        (false, Some(l)) => format!("{path}:{l}"),
        (false, None) => path.to_string(),
        (true, Some(l)) => format!("(unknown path):{l}"),
        (true, None) => "(unknown location)".to_string(),
    }
}

fn format_span_detail(span: &Value) -> Option<String> {
    let start_byte = span["start_byte"].as_u64()?;
    let end_byte = span["end_byte"].as_u64()?;
    let row = span["start_row"].as_u64();
    let col = span["start_col"].as_u64();
    let mut out = format!("Span: bytes {start_byte}-{end_byte}");
    if let Some(r) = row {
        let _ = write!(out, ", row {r}");
    }
    if let Some(c) = col {
        let _ = write!(out, ", col {c}");
    }
    Some(out)
}

fn format_source_block(path: &str, node: &Value) -> Option<String> {
    let source_lines = node["source_lines"].as_str();
    let source = node["source"].as_str();
    let body = source_lines.or(source)?;
    let truncated = node["source_truncated"].as_bool().unwrap_or(false);
    let tag = lang_tag(path);
    let mut out = String::new();
    let _ = writeln!(out, "```{tag}");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    if truncated {
        out.push_str("_source truncated_\n");
    }
    Some(out)
}

fn lang_tag(path: &str) -> &'static str {
    if path.ends_with(".blade.php") {
        "blade"
    } else if path.ends_with(".php") {
        "php"
    } else if path.ends_with(".tsx") {
        "tsx"
    } else if path.ends_with(".ts") {
        "ts"
    } else if path.ends_with(".jsx") {
        "jsx"
    } else if path.ends_with(".js") {
        "js"
    } else if path.ends_with(".py") {
        "python"
    } else {
        ""
    }
}

fn pagination_line(offset: usize, returned: usize, total: usize, truncated: bool) -> String {
    if total == 0 || returned == 0 {
        return format!("Showing 0 of {total}.");
    }
    let start = offset + 1;
    let end = offset.saturating_add(returned).min(total);
    let mut line = format!("Showing {start}-{end} of {total}.");
    if truncated {
        let next = offset.saturating_add(returned);
        let _ = write!(line, " Next offset: {next}.");
    }
    line
}

fn format_mib(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    format!("{mib:.1} MiB")
}

fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c);
    }
    out
}

fn pluralize(kind: &str, count: usize) -> String {
    if count == 1 {
        return kind.to_string();
    }
    if kind.ends_with('s') || kind.ends_with("ch") || kind.ends_with("sh") || kind.ends_with('x') {
        format!("{kind}es")
    } else if kind.ends_with('y')
        && kind.len() > 1
        && !matches!(
            kind.chars().nth_back(1).unwrap_or(' '),
            'a' | 'e' | 'i' | 'o' | 'u'
        )
    {
        format!("{}ies", &kind[..kind.len() - 1])
    } else {
        format!("{kind}s")
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

fn render_table(headers: &[&str], rows: &[Vec<String>], aligns: &[Align]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < cols {
                let w = cell.chars().count();
                if w > widths[i] {
                    widths[i] = w;
                }
            }
        }
    }

    let bar = |left: char, mid: char, right: char| -> String {
        let mut s = String::new();
        s.push(left);
        for (i, w) in widths.iter().enumerate() {
            for _ in 0..(w + 2) {
                s.push('─');
            }
            if i + 1 < cols {
                s.push(mid);
            }
        }
        s.push(right);
        s
    };

    let row_line = |cells: &[String]| -> String {
        let mut s = String::new();
        s.push('│');
        for (i, w) in widths.iter().enumerate() {
            let empty = String::new();
            let cell = cells.get(i).unwrap_or(&empty);
            let align = aligns.get(i).copied().unwrap_or(Align::Left);
            let _ = write!(s, " {} │", pad_cell(cell, *w, align));
        }
        s
    };

    let mut out = String::new();
    let _ = writeln!(out, "{}", bar('┌', '┬', '┐'));
    let header_cells: Vec<String> = headers.iter().map(|s| (*s).to_string()).collect();
    let _ = writeln!(out, "{}", row_line(&header_cells));
    let _ = writeln!(out, "{}", bar('├', '┼', '┤'));
    for row in rows {
        let _ = writeln!(out, "{}", row_line(row));
    }
    let _ = writeln!(out, "{}", bar('└', '┴', '┘'));
    out
}

fn pad_cell(s: &str, width: usize, align: Align) -> String {
    let len = s.chars().count();
    if len >= width {
        return s.to_string();
    }
    let pad = " ".repeat(width - len);
    match align {
        Align::Left => format!("{s}{pad}"),
        Align::Right => format!("{pad}{s}"),
    }
}

struct IdAppendix {
    seen: BTreeSet<String>,
    pairs: Vec<(String, String)>,
}

impl IdAppendix {
    fn new() -> Self {
        Self {
            seen: BTreeSet::new(),
            pairs: Vec::new(),
        }
    }

    fn add(&mut self, label: &str, id: &str) {
        if id.is_empty() {
            return;
        }
        if self.seen.insert(id.to_string()) {
            let l = if label.is_empty() { id } else { label };
            self.pairs.push((l.to_string(), id.to_string()));
        }
    }

    fn render(&self) -> String {
        if self.pairs.is_empty() {
            return String::new();
        }
        let mut out = String::from("\nNode IDs:\n");
        for (label, id) in &self.pairs {
            let _ = writeln!(out, "- {label} → {id}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta_current() -> Value {
        json!({
            "catching_up": false,
            "rss_bytes": 124_117_504_u64,
            "pending_paths": 0,
            "warnings": [],
        })
    }

    fn meta_catching_up(pending: u64) -> Value {
        json!({
            "catching_up": true,
            "rss_bytes": 124_117_504_u64,
            "pending_paths": pending,
            "warnings": [],
        })
    }

    #[test]
    fn status_renders_table_and_footer() {
        let result = json!({
            "files": 3410,
            "nodes": 35738,
            "symbols": 21106,
            "call_edges": 26473,
            "rss_bytes": 124_117_504_u64,
            "pending_paths": 0,
            "reconcile_done": true,
        });
        let out = render_tool_result(
            "status",
            &json!({"project_root": "/home/pat/projects/xrai"}),
            &result,
            &meta_current(),
        );
        assert!(out.starts_with("xgraph project: /home/pat/projects/xrai"));
        assert!(out.contains("## xgraph Status"));
        assert!(out.contains("Ready: yes."));
        assert!(out.contains("│ Files"));
        assert!(out.contains("│  3410 │"));
        assert!(out.contains("118.4 MiB"));
        assert!(out.contains("Index State: current."));
    }

    #[test]
    fn catching_up_footer_calls_out_pending_paths() {
        let result = json!({"files":0,"nodes":0,"symbols":0,"call_edges":0,"rss_bytes":0,"pending_paths":3,"reconcile_done":false});
        let out = render_tool_result("status", &json!({}), &result, &meta_catching_up(3));
        assert!(out.contains("Index State: catching up"));
        assert!(out.contains("3 pending paths"));
    }

    #[test]
    fn search_renders_hits_with_filters() {
        let args = json!({"name":"NavResolver","mode":"contains"});
        let result = json!({"hits":[{
            "node_id":"f9ce:1","name":"NavResolver","qname":"App\\Services\\NavResolver",
            "kind":"class","path":"app/Services/NavResolver.php"
        }]});
        let out = render_tool_result("search", &args, &result, &meta_current());
        assert!(out.contains("## Search: NavResolver"));
        assert!(out.contains("1 hit. Mode: contains."));
        assert!(out.contains("App\\Services\\NavResolver"));
        assert!(out.contains("f9ce:1"));
    }

    #[test]
    fn search_empty_includes_guidance() {
        let args = json!({"name":"Nope"});
        let result = json!({"hits":[]});
        let out = render_tool_result("search", &args, &result, &meta_current());
        assert!(out.contains("0 hits"));
        assert!(out.contains("mode=contains"));
    }

    #[test]
    fn context_empty_includes_mode_and_actionable_guidance() {
        let args = json!({"name":"NavResolver", "mode":"exact"});
        let result = json!({"matches":[], "total_matches":0, "limit":20});
        let out = render_tool_result("context", &args, &result, &meta_current());
        assert!(out.contains("Query: NavResolver"));
        assert!(out.contains("Mode: exact"));
        assert!(out.contains("Try `context` with `mode=contains`"));
    }

    #[test]
    fn find_symbol_empty_suggests_alternatives() {
        let args = json!({"name":"NavResolver"});
        let result = json!({"hits":[]});
        let out = render_tool_result("find_symbol", &args, &result, &meta_current());
        assert!(out.contains("No exact symbol found"));
        assert!(out.contains("mode=contains"));
    }

    #[test]
    fn nodes_in_file_groups_by_kind_and_pluralizes() {
        let args = json!({"path":"app/Services/NavResolver.php"});
        let result = json!({"nodes":[
            {"node_id":"f9ce:1","kind":"class","name":"NavResolver","qname":"App\\Services\\NavResolver"},
            {"node_id":"f9ce:3","kind":"method","name":"resolve","qname":"App\\Services\\NavResolver::resolve"},
            {"node_id":"f9ce:4","kind":"method","name":"resolveGroups","qname":"App\\Services\\NavResolver::resolveGroups"}
        ]});
        let out = render_tool_result("nodes_in_file", &args, &result, &meta_current());
        assert!(out.contains("3 nodes: 1 class, 2 methods."));
        assert!(out.contains("Class:"));
        assert!(out.contains("Methods:"));
        assert!(out.contains("f9ce:3"));
    }

    #[test]
    fn node_renders_calls_called_by_members_and_appendix() {
        let result = json!({"node":{
            "node_id":"f9ce:3","name":"resolve","qname":"App\\Services\\NavResolver::resolve",
            "kind":"method","path":"app/Services/NavResolver.php",
            "span":{"start_byte":100,"end_byte":200,"start_row":36,"start_col":4,"start_line":37},
            "source":"public function resolve() {}\n",
            "source_lines":"37\tpublic function resolve() {}\n",
            "source_truncated":false,
            "callers":["abcd:1"],
            "callees":["efgh:2","ijkl:3"],
            "caller_nodes":[{"node_id":"abcd:1","qname":"App\\Services\\Caller::call","name":"call","kind":"method","path":"app/Services/Caller.php"}],
            "callee_nodes":[
                {"node_id":"efgh:2","qname":"App\\Models\\Organization::enabledCapabilityKeys","name":"enabledCapabilityKeys","kind":"method","path":"app/Models/Organization.php"},
                {"node_id":"ijkl:3","qname":"App\\Services\\NavResolver::attachBadges","name":"attachBadges","kind":"method","path":"app/Services/NavResolver.php"}
            ],
            "member_nodes":[],
            "member_caller_nodes":[],
            "member_callee_nodes":[]
        }});
        let out = render_tool_result("node", &json!({}), &result, &meta_current());
        assert!(out.contains("## App\\Services\\NavResolver::resolve"));
        assert!(out.contains("method at app/Services/NavResolver.php:37"));
        assert!(out.contains("Node ID: f9ce:3"));
        assert!(out.contains("Span: bytes 100-200"));
        assert!(out.contains("Calls:"));
        assert!(out.contains("Called by:"));
        assert!(out.contains("```php"));
        assert!(out.contains("Node IDs:"));
        assert!(out.contains("efgh:2"));
        assert!(out.contains("abcd:1"));
    }

    #[test]
    fn callers_of_renders_table_and_pagination() {
        let args = json!({"node_id":"f9ce:3"});
        let result = json!({
            "node_ids":["abcd:1"],
            "nodes":[{"node_id":"abcd:1","qname":"App\\Services\\Caller::call","name":"call","kind":"method","path":"app/Services/Caller.php"}],
            "total": 1, "offset": 0, "limit": 200, "truncated": false
        });
        let out = render_tool_result("callers_of", &args, &result, &meta_current());
        assert!(out.contains("## Callers Of f9ce:3"));
        assert!(out.contains("1 caller."));
        assert!(out.contains("Showing 1-1 of 1."));
        assert!(out.contains("abcd:1"));
    }

    #[test]
    fn impact_groups_by_path_and_shows_depth() {
        let args = json!({"node_id":"f9ce:3","max_depth":2});
        let result = json!({
            "target":{"node_id":"f9ce:3","qname":"App\\Services\\NavResolver::resolve","name":"resolve","kind":"method","path":"app/Services/NavResolver.php"},
            "node_ids":["0300:42"],
            "nodes":[{"node_id":"0300:42","qname":"App\\Services\\SharedPageDataService::resolveNavigation","name":"resolveNavigation","kind":"method","path":"app/Services/SharedPageDataService.php"}],
            "total":1, "offset":0, "limit":500, "truncated":false
        });
        let out = render_tool_result("impact", &args, &result, &meta_current());
        assert!(out.contains("## Impact: App\\Services\\NavResolver::resolve"));
        assert!(out.contains("Depth: 2"));
        assert!(out.contains("app/Services/SharedPageDataService.php"));
        assert!(out.contains("0300:42"));
        assert!(out.contains("calls, renders, inherits, implements, references"));
    }

    #[test]
    fn trace_with_no_path_includes_guidance() {
        let args = json!({"from":"a","to":"b","max_depth":12});
        let result = json!({"path":null,"length":null});
        let out = render_tool_result("trace", &args, &result, &meta_current());
        assert!(out.contains("## Trace: a → b"));
        assert!(out.contains("No path found"));
        assert!(out.contains("max_depth=12"));
    }

    #[test]
    fn trace_renders_hops_with_node_ids() {
        let args = json!({"from":"a","to":"c"});
        let result = json!({
            "path":[
                {"node_id":"a","qname":"X::a","name":"a","kind":"method","path":"x.php"},
                {"node_id":"b","qname":"X::b","name":"b","kind":"method","path":"x.php"},
                {"node_id":"c","qname":"X::c","name":"c","kind":"method","path":"x.php"}
            ],
            "length": 2
        });
        let out = render_tool_result("trace", &args, &result, &meta_current());
        assert!(out.contains("## Trace: X::a → X::c"));
        assert!(out.contains("Shortest path: 2 hops."));
        assert!(out.contains("1. X::a"));
        assert!(out.contains("Node ID: a"));
        assert!(out.contains("calls → X::b"));
    }

    #[test]
    fn explore_groups_by_file_with_byte_summary() {
        let args = json!({"node_ids":["f9ce:3"]});
        let result = json!({
            "items":[{
                "node_id":"f9ce:3","name":"resolve","qname":"App\\Services\\NavResolver::resolve",
                "kind":"method","path":"app/Services/NavResolver.php",
                "span":{"start_byte":100,"end_byte":200,"start_row":36,"start_col":4,"start_line":37},
                "source":"public function resolve() {}\n",
                "source_lines":"37\tpublic function resolve() {}\n",
                "source_truncated":false,
                "caller_nodes":[],
                "callee_nodes":[]
            }],
            "bytes_used": 1967,
            "bytes_budget": 18000
        });
        let out = render_tool_result("explore", &args, &result, &meta_current());
        assert!(out.contains("1 node across 1 file. Used 1,967 / 18,000 bytes."));
        assert!(out.contains("### app/Services/NavResolver.php"));
        assert!(out.contains("#### App\\Services\\NavResolver::resolve — f9ce:3"));
        assert!(out.contains("```php"));
    }

    #[test]
    fn files_renders_pagination_and_prefix() {
        let args = json!({"prefix":"app/Services"});
        let result = json!({
            "files":["app/Services/Foo.php","app/Services/Bar.php"],
            "total": 309,
            "offset": 0,
            "limit": 2
        });
        let out = render_tool_result("files", &args, &result, &meta_current());
        assert!(out.contains("## Files: app/Services"));
        assert!(out.contains("Showing 1-2 of 309. Next offset: 2."));
        assert!(out.contains("- app/Services/Foo.php"));
    }

    #[test]
    fn pluralize_handles_common_kinds() {
        assert_eq!(pluralize("class", 2), "classes");
        assert_eq!(pluralize("method", 2), "methods");
        assert_eq!(pluralize("interface", 2), "interfaces");
        assert_eq!(pluralize("class", 1), "class");
    }

    #[test]
    fn table_lays_out_box_drawing() {
        let out = render_table(
            &["A", "B"],
            &[vec!["1".into(), "two".into()]],
            &[Align::Right, Align::Left],
        );
        assert!(out.contains("┌"));
        assert!(out.contains("┐"));
        assert!(out.contains("└"));
        assert!(out.contains("┘"));
        assert!(out.contains("│ A │   B │") || out.contains("│ A │ B   │"));
    }
}
