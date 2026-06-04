# xgraph

A Rust-native code graph for large Git worktrees, built for agent clients.
xgraph indexes your repo into an embedded [CozoDB](https://docs.cozodb.org/en/latest/stored.html)
graph, keeps it fresh through an on-demand daemon, and serves fast structural
queries — symbols, callers, callees, impact, traces — over MCP.

Linux only. PHP, Laravel/Blade, TypeScript, JavaScript, TSX, and Python.

> Inspired by [`colbymchenry/codegraph`](https://github.com/colbymchenry/codegraph),
> with a different runtime model: one daemon per worktree owns watching, parsing,
> writes, hot indexes, and request dispatch.

## Install

```bash
git clone https://github.com/nullbio/xgraph
cd xgraph
./install.sh
```

`install.sh` wraps `cargo install --git . --force` and sets the compiler flag
needed by Cozo/RocksDB on GCC 13+. Re-run any time to upgrade to latest
`master`.

Direct install with Cargo:

```bash
CXXFLAGS="-include cstdint" cargo install --git https://github.com/nullbio/xgraph --force
```

## Quick start

From inside any Git worktree:

```bash
xgraph init
```

`init` indexes the repo and — if Claude Code or Codex is installed — offers to
register xgraph as a global MCP server for you. Accept the prompt and you're
done: the right worktree daemon lazy-starts the next time an agent makes a
query.

## Features

- Cross-file linking across PHP, Blade, TypeScript, JavaScript, TSX, and Python.
- **Laravel resolution**: routes, Eloquent relationships, facades, container
  bindings, events, jobs, controller-to-model edges, and Blade references.
- **React resolution**: function and class components, custom hooks, builtin
  hooks, `memo`, and `forwardRef`.
- **Python resolution**: package-relative imports for cross-file calls.
- **Live freshness**: filesystem watching reconciles the graph after edits,
  deletes, renames, ignore-file changes, and branch checkouts.
- **Hot in-memory indexes** for exact lookup, prefix/contains search,
  file-scoped lookup, callers/callees, focused context, multi-node source
  exploration, and shortest-path tracing.
- **Cozo Datalog** for transitive impact, cycles, dependency cones, and
  module-boundary checks.
- **Ignore-aware**: honours `.gitignore`, Git excludes, and `.xgraphignore` for
  every code path (scan, watcher, sync, reindex, startup recovery).

## MCP tools

Once registered, agents get these tools. Every tool call includes a
`project_root`; the MCP proxy resolves that path to a canonical Git worktree
and routes the request to that worktree's daemon.

| Tool | Purpose |
| --- | --- |
| `find_symbol` | Exact symbol lookup. |
| `search` | Prefix or contains search, optionally path-scoped. |
| `node` | Source and metadata for a node ID. |
| `nodes_in_file` | All symbols/nodes in a file. |
| `callers_of` / `callees_of` | Direct caller/callee lookup. |
| `context` | Focused context for a symbol. |
| `explore` | Source for several node IDs under a shared byte budget. |
| `impact` | What would be affected if a node changed. |
| `trace` | Shortest call path between two nodes. |
| `files` | Indexed file listing. |
| `status` | Graph totals and daemon health. |

Every response starts with the routed xgraph project path and carries metadata
— catch-up state, daemon memory, queued paths, warnings — so clients can tell
which worktree answered and whether the graph is still converging.

## CLI

The CLI is a thin client over the same daemon socket; commands below mirror the
MCP tools and add maintenance verbs.

### Maintenance

```bash
xgraph init        # create schema, scan, index, offer MCP registration
xgraph status      # file/node/symbol/edge counts and daemon health
xgraph sync        # reconcile the manifest with files on disk
xgraph reindex     # rebuild the graph
xgraph daemon stop # stop the per-worktree daemon
```

All commands default to the current directory's Git worktree. Pass
`--project-root <path>` before the subcommand to operate on another worktree
without changing directories.

`init` uses the live daemon when one is reachable so connected MCP clients keep
their socket transport; with no daemon running, it performs direct store
maintenance. `status`, `sync`, `reindex`, and query commands start the
worktree daemon on demand when no reachable daemon socket exists.

The daemon starts automatically when the first CLI or MCP graph request arrives.
`xgraph daemon start` exists if you want to warm it up explicitly. Daemons exit
after 15 minutes without received commands and no in-flight command, and also
exit when their worktree root or persistent xgraph store path disappears.

### Queries

```bash
xgraph find-symbol User --kind class
xgraph search User --mode prefix
xgraph search Controller --mode contains --path-prefix app/Http
xgraph callers <node-id> --limit 50
xgraph callees <node-id> --limit 50
xgraph impact <node-id> --max-depth 3 --limit 100
xgraph context UserService --path-prefix app/Services
xgraph trace <from-id> <to-id>
xgraph files --prefix app/Services --limit 50
```

## Branch checkouts and Git worktrees

xgraph treats the graph as the current files on disk. The branch name is
metadata.

**Ordinary checkout** in the same directory:

```bash
git checkout feature
```

The watcher sees the change, the daemon batches and reconciles, unchanged
content is reused via content hashing, and changed content is parsed. Active
graph rows are replaced transactionally — readers never see a half-updated
graph.

**Linked worktrees** when you want two branches queryable at once:

```bash
git worktree add ../repo-feature feature
cd ../repo-feature
xgraph init
```

Each linked worktree gets its own database and daemon. A single `xgraph mcp`
proxy can query multiple linked worktrees by sending different `project_root`
values in different tool calls; the proxy keeps separate daemon connections and
reconnects if a cached socket goes stale.

## How it works

One daemon per worktree owns:

- the filesystem watcher and debounce queue
- parser workers and the shared compiled Tree-sitter queries
- the embedded Cozo connection and single writer queue
- in-memory hot indexes
- MCP request dispatch and maintenance commands

`xgraph mcp` proxies are intentionally lightweight: they answer the MCP
handshake locally, resolve each tool call's `project_root`, lazy-connect to
that worktree's daemon when graph data is needed, and proxy JSON-RPC over the
daemon socket. If a cached daemon connection is stale, the proxy reconnects and
retries the request once.

### Storage

Persistent state lives under the worktree's private Git directory:

```text
$(git rev-parse --git-path xgraph)/
  config.toml
  graph.cozo/
  schema.version
```

Runtime files use a short disposable path to stay under Unix socket length
limits:

```text
${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/
  xgraph.sock
  startup.lock
  daemon.lock
  daemon.pid
```

### Core decisions

| Concern | Decision |
| --- | --- |
| Platform | Linux |
| Database | Embedded CozoDB |
| Scope | One database per Git worktree |
| Runtime owner | One on-demand, self-reaping daemon per worktree |
| Agent access | Many `xgraph mcp` proxy processes |
| Update source | Filesystem watcher plus manifest reconciliation |
| Branch model | Graph = current files on disk; branch name is metadata |
| Parser API | Rust `tree-sitter` crate, natively linked |
| Indexed file set | Non-ignored worktree files after Git rules and `.xgraphignore` |

## Language support

Tree-sitter provides syntax. xgraph extractors and resolver passes add language
and framework meaning: definitions, imports/exports, classes, traits, enums,
routes, calls, member chains, framework-derived edges, and diagnostics.

Framework-derived edges carry provenance and confidence. Synthetic framework
nodes use the `lh:` prefix — e.g. `lh:route:get /users`,
`lh:UserController::index`, `lh:react.component`, `lh:react.hook.useState`.

## Development

```bash
just check
```

Or, without `just`:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Benchmarks under `benches/` cover parsing, initial indexing, content-hash
skipping, hot-index loading, and per-phase index timing.

## References

- [CozoDB stored relations](https://docs.cozodb.org/en/latest/stored.html)
- [Rust Tree-sitter parser API](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Parser.html)
- [Tree-sitter advanced parsing](https://tree-sitter.github.io/tree-sitter/using-parsers/3-advanced-parsing.html)
- Grammar crates:
  [JavaScript](https://docs.rs/tree-sitter-javascript/latest/tree_sitter_javascript/),
  [TypeScript/TSX](https://docs.rs/tree-sitter-typescript/latest/tree_sitter_typescript/),
  [PHP](https://docs.rs/tree-sitter-php/latest/tree_sitter_php/),
  [Python](https://docs.rs/tree-sitter-python/latest/tree_sitter_python/),
  [Blade](https://github.com/EmranMR/tree-sitter-blade)
