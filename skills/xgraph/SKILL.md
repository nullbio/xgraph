---
name: xgraph
description: "Use when working with xgraph code intelligence or the xgraph Rust repo: querying symbols, files, callers, callees, impact, traces, or focused context; diagnosing xgraph daemon/MCP behavior; deciding when to use xgraph versus grep or LSP; handling project_root/worktree routing; or updating xgraph docs, tool definitions, install scripts, or skills."
---

# xgraph

xgraph is a Rust-native, daemon-backed code graph for Git worktrees. Use it for fast structural questions about indexed code. Do not treat it as an LSP, refactor engine, or diagnostics source.

## Operating Model

- Always identify the intended Git worktree. xgraph state is per worktree, not per branch name.
- Every MCP tool call must include `project_root`, a path inside the target Git worktree.
- A single MCP proxy can query multiple worktrees by sending different `project_root` values.
- The daemon for a worktree starts on demand, exits after 15 minutes with no received commands and no in-flight command, and exits when its worktree or persistent store path disappears.
- xgraph output should show the routed project path first. If the path is not what you expected, stop and correct the query context before trusting the result.

## Tool Choice

Prefer xgraph for structural graph questions:

- symbol lookup: `find_symbol`, `search`
- file inventory: `files`, `nodes_in_file`
- source and metadata for known nodes: `node`, `explore`
- relationship questions: `callers_of`, `callees_of`, `impact`, `trace`
- focused orientation around one symbol: `context`
- index health and freshness: `status`

Prefer raw file search/read for literal strings, comments, logs, config, docs, or unindexed files.

Prefer LSP tools for hover/type information, diagnostics, definitions, implementations, rename/refactor operations, and post-edit verification.

## Query Workflow

1. Start with `status` for the target `project_root` when freshness matters or results look surprising.
2. Use `search` or `find_symbol` to get node IDs. For methods, search exact qualified names such as `Class::method` when available.
3. Use node IDs for graph operations: `node`, `explore`, `callers_of`, `callees_of`, `impact`, and `trace`.
4. Use `context` for quick orientation, but cross-check broad/common names before making code changes from it.
5. Use `explore` instead of many individual `node` calls when reading several related nodes.
6. If `catching_up`, pending paths, or warnings appear, wait or rerun after reconciliation before making strong claims.

## Accuracy Rules

- Treat exact symbol, file, node, and trace results as stronger than broad caller/callee/impact/context results.
- Common names can over-broaden graph relationships. Cross-check high-impact decisions with source or LSP.
- xgraph reflects current files on disk in the selected worktree. A same-directory `git checkout` is filesystem churn in the same database; a linked `git worktree` gets a separate database and daemon.
- Do not initialize xgraph for a project unless the user asks or project instructions allow it. If a project is not initialized, ask before running `xgraph init`.

## Repo Work

When editing the xgraph repo:

- Keep MCP tool descriptions and schemas in `src/mcp_protocol.rs` aligned with README and skill guidance.
- Update `README.md` and `IMPLEMENTATION_GUIDE.md` when daemon, runtime, storage, MCP, or worktree behavior changes.
- Use `just check` for verification. If `just` is unavailable, run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

- Install from the local repo when validating unpushed changes:

```bash
env CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-include cstdint" cargo install --path . --force
```
