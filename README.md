# xgraph

xgraph is a Linux-only, Rust-native code graph service for large local Git
worktrees. It indexes source code into an embedded
[CozoDB](https://docs.cozodb.org/en/latest/stored.html) graph, keeps that graph
fresh through a long-lived daemon, and exposes fast code-intelligence queries to
agent clients through lightweight MCP proxy processes.

The project is inspired by
[`colbymchenry/codegraph`](https://github.com/colbymchenry/codegraph) and uses a
different runtime model: one daemon owns file watching, parsing, graph updates,
Cozo writes, hot indexes, and request dispatch for each Git worktree.

## What works today

xgraph runs end-to-end with cross-file linking across PHP, Blade, TypeScript,
JavaScript, TSX, and Python.

- `xgraph init` indexes a Git worktree into Cozo and reports progress plus graph
  totals.
- `xgraph daemon start` opens a Unix socket and serves MCP/query requests from
  in-memory hot indexes loaded from the persistent graph.
- `xgraph mcp` answers MCP handshake methods locally, lazy-starts the daemon for
  graph tool calls, and proxies MCP-style JSON-RPC.
- `xgraph init`, `xgraph sync`, and `xgraph reindex` use the live daemon when it
  is available, so terminal maintenance keeps connected MCP clients on the same
  transport.
- Filesystem watching keeps the active graph current after edits, deletes,
  renames, ignore-file changes, and branch checkouts.
- Changed files are currently parsed as whole files. Incremental graph updates,
  content-hash skipping, and batched Cozo writes are active; Tree-sitter
  old-tree incremental parsing remains a benchmarked optimization path.
- Laravel framework resolution covers routes, Eloquent relationships, facades,
  service container bindings, events, jobs, controller-to-model edges, and Blade
  references.
- React framework resolution covers function components, class components,
  custom hooks, builtin hook calls, `memo`, and `forwardRef`.
- Python support uses the native `tree-sitter-python` grammar and resolves
  package-relative imports for cross-file calls.

## Core decisions

| Concern | Decision |
| --- | --- |
| Platform | Linux |
| Database | Embedded CozoDB |
| Project type | Git worktrees |
| Scope | One database per Git worktree |
| Runtime owner | One daemon per worktree |
| Agent access | Many `xgraph mcp` proxy processes |
| Update source | Filesystem watcher plus manifest reconciliation |
| Branch model | Graph represents current files on disk, with branch name as metadata |
| Persistent state | Worktree-private Git path from `git rev-parse --git-path xgraph` |
| Runtime files | Short path under `${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/` |
| Parser API | Rust `tree-sitter` crate |
| Parser deployment | Native, statically linked grammar crates |
| Indexed file set | Current non-ignored worktree files after Git ignore rules and `.xgraphignore` |

CozoDB provides durable graph facts, transactions, snapshots, and complex Datalog
queries. xgraph owns file watching, daemon lifecycle, worktree discovery, parser
scheduling, hot indexes, socket/proxy transport, and agent coordination.

## Install

```bash
git clone https://github.com/nullbio/xgraph
cd xgraph
./install.sh
```

The install script wraps `cargo install --git . --force` and sets the compiler
flag needed by the current Cozo/RocksDB dependency stack on newer GCC releases.
Run it again any time to install the latest `master`.

You can also install directly with Cargo:

```bash
CXXFLAGS="-include cstdint" cargo install --git https://github.com/nullbio/xgraph --force
```

## Quick start

Run xgraph from inside a Git worktree:

```bash
xgraph init
xgraph status
```

Then connect agents through MCP:

```bash
xgraph mcp
```

For a warm daemon before agents connect:

```bash
xgraph daemon start
```

## Commands

### Maintenance

```bash
xgraph init
xgraph status
xgraph sync
xgraph reindex
xgraph daemon start
xgraph daemon stop
```

- `init` creates the Cozo schema, records project metadata, performs the initial
  scan/index, and writes config.
- `status` reports indexed file, node, symbol, and edge counts plus daemon
  health.
- `sync` reconciles the manifest with the current files on disk.
- `reindex` rebuilds the graph.
- `daemon start` and `daemon stop` manage the per-worktree daemon.

When a daemon is reachable, `init`, `sync`, and `reindex` are sent to that daemon
so connected MCP clients keep their socket transport. When no daemon is
reachable, those commands perform direct store maintenance after clearing stale
runtime state.

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

Query subcommands are thin clients. They send JSON-RPC requests to the daemon
socket and pretty-print the response. The daemon is the source of truth for
extraction, graph updates, and indexes.

The same capabilities are exposed through MCP tools for agents:

- `find_symbol`
- `search`
- `node`
- `nodes_in_file`
- `callers_of`
- `callees_of`
- `context`
- `explore`
- `impact`
- `trace`
- `files`
- `status`

Every response includes metadata such as catch-up state, daemon memory usage,
queued paths, and warnings. While a branch checkout, edit burst, or startup
reconcile is still being processed, queries can surface `catching_up` so clients
know the graph is still converging.

## Branch checkouts and Git worktrees

xgraph supports both ordinary branch checkouts in one directory and linked Git
worktrees.

An ordinary branch checkout keeps the same worktree directory:

```bash
cd repo
git checkout feature
```

xgraph treats that as a large filesystem change in the existing worktree, using
the same database and daemon identity:

```text
checkout branch
  -> watcher sees many changes
  -> daemon batches/reconciles
  -> changed paths are hashed
  -> unchanged content is reused
  -> changed content is parsed
  -> active graph rows are replaced transactionally
```

The branch name is metadata. The graph identity is the current worktree root plus
the current file manifest. Checking out `master` or another branch in the same
directory reconciles the same graph to the files currently on disk.

A linked Git worktree gives another branch its own directory:

```bash
git worktree add ../repo-feature feature
cd ../repo-feature
xgraph init
```

Each linked worktree gets its own database and long-lived daemon. Use linked
worktrees when two branches need to be queryable at the same time.

## Project discovery and storage

xgraph runs inside Git worktrees. A supported invocation resolves:

1. The canonical Git worktree root.
2. The worktree-private Git storage path:

   ```bash
   git rev-parse --git-path xgraph
   ```

3. The short runtime path derived from the canonical worktree root hash.

Persistent state lives in the worktree's private Git directory:

```text
$(git rev-parse --git-path xgraph)/
  config.toml
  graph.cozo/
  schema.version
```

Runtime files use a short disposable path to avoid Linux Unix socket length
limits:

```text
${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/
  xgraph.sock
  startup.lock
  daemon.lock
  daemon.pid
```

Git is used for discovery and private storage placement. Freshness is maintained
through filesystem watching and manifest reconciliation.

## Ignore policy

xgraph indexes the current non-ignored files in the Git worktree. Initial scan,
watcher ingestion, `xgraph sync`, `xgraph reindex`, and startup crash recovery
all use the same ignore matcher.

The ignore matcher combines:

1. Built-in exclusions for `.git`, xgraph persistent state, dependency
   directories, build outputs, and disposable runtime paths.
2. Git ignore rules, including worktree `.gitignore` files and Git exclude
   sources.
3. Optional `.xgraphignore` files using Gitignore-compatible syntax for
   xgraph-only exclusions.

Ignored paths stay out of the graph: they are outside the manifest, hashing,
parsing, and Cozo writes. When an ignore file changes, the daemon rebuilds the
matcher and reconciles the manifest.

## Runtime model

Each worktree daemon owns:

- Filesystem watcher.
- Debounce and batch queue.
- Parser workers.
- Language registry.
- Shared compiled `Arc<Query>` values per language/extractor.
- Embedded Cozo connection.
- Single writer queue.
- In-memory hot indexes.
- MCP request dispatcher.
- Maintenance command channel for daemon-owned `sync` and `reindex`.

MCP proxy processes are intentionally lightweight. They answer the initial MCP
handshake locally, lazy-connect to the worktree daemon when graph data is needed,
and proxy requests over the daemon socket.

## Language support

Core language support is focused on:

- PHP.
- Laravel framework conventions, including Blade.
- TypeScript, JavaScript, and TSX.
- Python.

Tree-sitter provides syntax. xgraph extractors and resolver passes add language
and framework meaning: definitions, imports, exports, classes, traits, enums,
route declarations, call expressions, member/property chains, framework-derived
edges, and diagnostics.

Framework-derived edges carry provenance and confidence. Synthetic framework
nodes use the `lh:` prefix, such as `lh:route:get /users`,
`lh:UserController::index`, `lh:react.component`, and
`lh:react.hook.useState`.

## Query strategy

Hot MCP calls use in-memory indexes:

- Exact symbol lookup.
- Prefix and contains search.
- File-scoped node lookup.
- Caller and callee lookup by node ID.
- Focused context for a symbol or path.
- Multi-node source exploration.
- Shortest call-path tracing.
- Indexed file listing.
- Daemon status.

Cozo Datalog is used for broader graph analysis such as transitive impact,
cycle detection, dependency cones, path queries, module boundary checks, and
"what changes if X changes?" workflows.

Simple lookups stay in memory. Broad analysis uses the durable graph.

## Development

For local verification, run:

```bash
just check
```

If `just` is unavailable, run the underlying commands directly:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Benchmarks live under `benches/` and cover parsing, initial indexing,
content-hash skipping, hot-index loading, and per-phase index timing.

## References

- Cozo stored relations: <https://docs.cozodb.org/en/latest/stored.html>
- Rust Tree-sitter parser API: <https://docs.rs/tree-sitter/latest/tree_sitter/struct.Parser.html>
- Rust Tree-sitter query cursors: <https://docs.rs/tree-sitter/latest/tree_sitter/struct.QueryCursor.html>
- Rust Tree-sitter tree editing and changed ranges: <https://docs.rs/tree-sitter/latest/tree_sitter/struct.Tree.html>
- Tree-sitter advanced parsing: <https://tree-sitter.github.io/tree-sitter/using-parsers/3-advanced-parsing.html>
- JavaScript grammar crate: <https://docs.rs/tree-sitter-javascript/latest/tree_sitter_javascript/>
- TypeScript/TSX grammar crate: <https://docs.rs/tree-sitter-typescript/latest/tree_sitter_typescript/>
- PHP grammar crate: <https://docs.rs/tree-sitter-php/latest/tree_sitter_php/>
- Python grammar crate: <https://docs.rs/tree-sitter-python/latest/tree_sitter_python/>
- Blade grammar: <https://github.com/EmranMR/tree-sitter-blade>
