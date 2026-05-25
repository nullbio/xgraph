# xgraph

xgraph is a Linux-only, Rust-native code graph service for large local Git worktrees. It indexes source code into a durable embedded [CozoDB](https://docs.cozodb.org/en/latest/stored.html) graph, keeps that graph fresh through a single long-lived daemon, and exposes fast code-intelligence queries to many agent clients through lightweight MCP proxy processes.

The project is inspired by [`colbymchenry/codegraph`](https://github.com/colbymchenry/codegraph), but it is not a direct port. xgraph uses a different runtime model: one daemon owns file watching, parsing, graph updates, Cozo writes, hot indexes, and request dispatch for each Git worktree.

## Goals

- Better Linux compatibility than the original project.
- Rust implementation with aggressive attention to latency, throughput, and memory use.
- Embedded CozoDB as the durable per-worktree graph database.
- One daemon per worktree, shared by many MCP clients.
- Native, statically linked Tree-sitter grammars on the parsing hot path.
- Fast incremental updates from filesystem events plus manifest reconciliation.
- Correct behavior across branch checkouts and Git worktrees.
- Initial language focus:
  - PHP
  - Laravel framework conventions, including Blade
  - TypeScript / JavaScript
  - Python

## Non-goals

- Non-Linux support.
- SQLite support.
- One database shared by multiple worktrees.
- Multiple independent watchers or DB writers for the same worktree.
- Treating Git branch name as the graph identity.
- Supporting projects without a `.git` directory.
- Shelling out to the Tree-sitter CLI on the hot path.
- WASM parsers on the hot path.

## Core decisions

| Concern | Decision |
| --- | --- |
| Database | Embedded CozoDB |
| Scope | One DB per Git worktree |
| Runtime owner | One daemon per worktree |
| Agent access | Many `xgraph mcp` proxy processes |
| Update source | Filesystem watcher plus manifest reconciliation |
| Branch model | DB represents current files on disk, not a branch name |
| Git dependency | Required for project discovery/storage; not required for freshness after startup |
| Persistent state | Worktree-private Git path from `git rev-parse --git-path xgraph` |
| Runtime files | Short path under `${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/` |
| Parser API | Rust `tree-sitter` crate directly |
| Parser deployment | Native, statically linked grammar crates |
| Parser workers | Fixed daemon-owned worker pool |
| Parser cache | Content-hash skip cache keyed by parser version |
| Indexed file set | Current non-ignored worktree files after Git ignore rules and `.xgraphignore` |

CozoDB is responsible for durable graph facts, transactions, snapshots, and complex Datalog queries. xgraph owns file watching, daemon lifecycle, worktree discovery, parser scheduling, hot indexes, socket/proxy transport, and agent coordination.

## Command model

```bash
xgraph init
```

Creates the Cozo schema, records project metadata, performs the initial scan/index, and writes config. It exits when initialization is complete.

```bash
xgraph mcp
```

Primary command for agents. It resolves the current Git worktree, ensures the daemon is running, then proxies MCP stdin/stdout to the daemon socket.

```bash
xgraph daemon start
```

Manually starts the daemon. Useful for debugging or for keeping the graph warm before agents connect.

```bash
xgraph status
xgraph daemon stop
xgraph sync
xgraph reindex
```

Operational commands for inspecting state, stopping the daemon, reconciling the manifest with disk, and rebuilding the graph.

## Project discovery

xgraph only supports Git projects. Commands run outside a Git worktree should exit cleanly with an explanation and should not create local state.

A supported invocation resolves:

1. The canonical Git worktree root.
2. The worktree-private Git storage path:
   ```bash
   git rev-parse --git-path xgraph
   ```
3. The short runtime path derived from the canonical worktree root hash.

Git is used for discovery and private storage placement. Freshness is maintained by xgraph through filesystem watching and manifest reconciliation, not by polling branch names.

## Ignore policy

xgraph indexes the current non-ignored files in the Git worktree. Initial scan, watcher ingestion, `xgraph sync`, `xgraph reindex`, and startup crash recovery must all use the same ignore matcher.

The ignore matcher combines:

1. Built-in exclusions for `.git`, xgraph persistent state, dependency directories, build outputs, and disposable runtime paths.
2. Git ignore rules, including worktree `.gitignore` files and Git exclude sources.
3. Optional `.xgraphignore` files using Gitignore-compatible syntax for xgraph-only exclusions.

`.xgraphignore` is project input, not xgraph state. xgraph may read it whether the project chooses to track it or keep it local, but xgraph must not create it as part of normal operation.

Ignored paths are out of the graph: they are not part of the manifest, are not hashed, are not parsed, and are not written to Cozo. The watcher may observe events for ignored paths because it watches directories, but the debounce queue must filter those events before file reads or parser work. If an ignore file changes, the daemon rebuilds the matcher and reconciles the manifest; files that became ignored are removed from active graph rows transactionally just like deletions.

## Storage layout

Persistent state lives in the worktree's private Git directory, never in tracked project files:

```text
$(git rev-parse --git-path xgraph)/
  config.toml
  graph.cozo/
  schema.version
```

Runtime files use a short path to avoid Linux Unix socket length limits:

```text
${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/
  xgraph.sock
  startup.lock
  daemon.lock
  daemon.pid
```

Runtime files are disposable and may be recreated. PID files are diagnostic only. Correct ownership is enforced with OS-level locks.

## Daemon startup

`xgraph mcp` lazily starts the daemon:

1. Resolve the Git worktree root.
2. Compute the short runtime directory.
3. Ping `xgraph.sock`.
4. If alive, connect.
5. Acquire `startup.lock`.
6. Ping the socket again.
7. If still dead, remove stale socket and PID files.
8. Spawn the daemon.
9. Wait for socket ping.
10. Proxy MCP traffic.

The daemon holds `daemon.lock` for its entire lifetime. If the daemon crashes, the OS releases the lock, so there is no permanent stale lockout.

## Runtime ownership

The daemon owns:

- Filesystem watcher.
- Debounce and batch queue.
- Fixed parser worker pool.
- Language registry.
- Shared compiled `Arc<Query>` values per language/extractor.
- Embedded Cozo connection.
- Single writer queue.
- In-memory hot indexes.
- MCP request dispatcher.

Each parser worker owns:

- A `Parser` per language.
- A `QueryCursor` per language or request.
- Scratch buffers.

All agents for a worktree share that daemon. There must not be multiple watchers, parser pools, or database writers for the same worktree.

## Parser architecture

The hot path is:

```text
Rust daemon
  -> native statically linked Tree-sitter grammars
  -> parser worker pool
  -> per-language compiled queries/extractors
  -> content-hash skip cache
  -> incremental parse only when old text + old tree are available
  -> Cozo write queue
```

Use the Rust `tree-sitter` crate directly. Do not shell out to the `tree-sitter` CLI, and do not use WASM parsers on the hot path.

Initial grammar decisions:

```toml
tree-sitter = "0.26"
tree-sitter-javascript = "0.25"
tree-sitter-typescript = "0.23"
tree-sitter-php = "0.24"
```

Python remains a required core language, but its native grammar crate/version must be selected and pinned before Python extractor implementation.

Blade should use a vendored native [`tree-sitter-blade`](https://github.com/EmranMR/tree-sitter-blade) grammar. Treating `.blade.php` as plain PHP is not good enough for Laravel support.

## Initial indexing

Initial scan should be embarrassingly parallel:

1. Walk non-ignored files with the shared Git/`.xgraphignore` matcher.
2. Detect language by path and extension.
3. Hash file bytes.
4. If the content hash and parser version are already extracted, reuse facts.
5. Otherwise parse in the worker pool.
6. Extract nodes, refs, imports, calls, framework facts, and diagnostics.
7. Batch Cozo writes through the single writer queue.

The fastest parse is no parse. Content hashing matters more than micro-optimizing Tree-sitter when switching branches or worktrees.

## Realtime edits

For file changes:

1. Recheck the path with the shared ignore matcher after debounce.
2. If the path is ignored, remove any active rows for that path and skip file reads.
3. If an ignore file changed, rebuild the matcher and enqueue manifest reconciliation.
4. Read final file bytes.
5. Hash bytes.
6. If the hash is unchanged, skip.
7. If old bytes and old tree are available:
   - Compute the smallest single replacement range.
   - Apply `old_tree.edit(...)`.
   - Parse with `parser.parse(new_bytes, Some(&old_tree))`.
   - Use `changed_ranges` to limit re-extraction where it is safe.
8. Otherwise, perform a full-file parse.

Filesystem watchers do not provide reliable text edit ranges. Incremental parsing only works when xgraph keeps old bytes and computes the diff itself. Branch checkouts and formatter rewrites will often be full-file parses, which is acceptable when content hashing and batching are correct.

## Extraction strategy

Tree-sitter provides syntax. Language and framework meaning comes from xgraph extractors and resolver passes.

Use small precise queries for:

- Definitions.
- Imports.
- Exports.
- Classes, interfaces, traits, and enums.
- Route declarations.

Use manual cursor traversal for:

- Call expressions.
- Member and property chains.
- Nested scopes.
- Laravel-specific heuristics.

Avoid broad "match everything" queries. Use byte-range-limited queries for incremental extraction. Avoid repeated `node.utf8_text()` calls; slice bytes directly and intern common names.

## Laravel and PHP

Parsing choices:

- `.php` uses the PHP/PHP-only grammar.
- `.blade.php` uses the Blade grammar, plus embedded PHP/HTML/JS ranges where useful.
- Laravel-significant paths get resolver attention:
  - `routes/*.php`
  - `app/Http/Controllers`
  - `app/Models`
  - `database/migrations`

Laravel-specific resolution should model:

- `Route::get(..., [Controller::class, 'method'])` and related route forms.
- Controller method to model calls.
- Eloquent relationships.
- Facades.
- Service container bindings.
- Events, listeners, and jobs.
- Blade view references.

Tree-sitter gets syntax nodes. Laravel meaning comes from a framework resolver pass. Framework-derived edges must carry explicit provenance and confidence.

Framework-edge node IDs use the synthetic `lh:` prefix (e.g. `lh:route:get /users`, `lh:UserController::index`) so they cannot collide with parser-extracted IDs (which always start with a 64-character content hash). MCP clients reading framework edges via `callers_of`/`callees_of` should treat `lh:*` IDs as synthesis points: they do not appear in `active_node` and are not directly queryable by `nodes_in_file`. Edge provenance is `"laravel_heuristic"` and confidence ranges 40 (low) — 70 (medium) — 90 (high) depending on the pattern.

## Language growth

Core languages should be native/static and reproducible. Add long-tail languages later behind feature-gated grammar crates or language packs only after the initial languages work well.

The plugin boundary should look like:

```rust
trait LanguagePlugin {
    fn id(&self) -> LanguageId;
    fn extensions(&self) -> &[&str];
    fn tree_sitter_language(&self) -> Language;
    fn queries(&self) -> &'static LanguageQueries;
    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile;
}
```

Keep language-specific extraction separate from active graph materialization and cross-file resolution.

## Branch and worktree behavior

A branch checkout in the same worktree is a large filesystem change, not a new database identity.

```text
checkout branch
  -> watcher sees many changes
  -> daemon batches/reconciles
  -> changed paths are hashed
  -> unchanged content is reused
  -> changed content is parsed
  -> active graph rows are replaced transactionally
```

The branch name is metadata. The graph identity is the current worktree root plus the current file manifest.

A new Git worktree gets a new database:

```bash
git worktree add ../repo-feature feature
cd ../repo-feature
xgraph init
```

Cross-worktree reuse can be added later through an optional global content cache. The first version prioritizes isolation and correctness.

## Cozo schema shape

Parsed content is separate from active workspace state.

```text
content_file[content_hash] => language, parser_version, diagnostics
content_node[content_hash, local_node_id] => kind, name, qname, span
content_ref[content_hash, local_ref_id] => kind, name, span

active_file[path] => content_hash, mtime, size, generation
active_node[node_id] => path, content_hash, local_node_id, kind, name, qname, span

edge[source_node_id, kind, target_node_id] => provenance, confidence
symbol[name, kind, node_id] => qname, path
```

A file update is one transaction:

1. Remove active rows for the path.
2. Insert the new active file row.
3. Materialize active nodes from content facts.
4. Resolve references.
5. Insert edges.
6. Commit.

Readers see either the old committed graph or the new committed graph, never a half-updated mixture.

## Crash recovery

On daemon startup:

1. Open Cozo.
2. Load the manifest.
3. Scan current non-ignored files cheaply.
4. Compare path, hash, mtime, and size.
5. Enqueue dirty, missing, and deleted files.
6. Serve reads with `status = catching_up`.
7. Commit repairs incrementally.

If a crash happens during an update, Cozo rolls back the incomplete transaction. The next startup scan catches stale paths.

## Query strategy

Use in-memory indexes for hot MCP calls:

- Node by ID.
- Callers.
- Callees.
- Files.
- Simple symbol lookup.

Use Cozo Datalog for complex graph queries:

- Transitive impact.
- Cycles.
- Dependency cones.
- Path queries.
- Module boundary checks.
- "What changes if X changes?"

Simple requests should not pay for the general graph query engine. Complex graph analysis should use Cozo instead of being hand-rolled in ad hoc indexes.

## References

- Cozo stored relations: <https://docs.cozodb.org/en/latest/stored.html>
- Rust Tree-sitter parser API: <https://docs.rs/tree-sitter/latest/tree_sitter/struct.Parser.html>
- Rust Tree-sitter query cursors: <https://docs.rs/tree-sitter/latest/tree_sitter/struct.QueryCursor.html>
- Rust Tree-sitter tree editing and changed ranges: <https://docs.rs/tree-sitter/latest/tree_sitter/struct.Tree.html>
- Tree-sitter advanced parsing: <https://tree-sitter.github.io/tree-sitter/using-parsers/3-advanced-parsing.html>
- JavaScript grammar crate: <https://docs.rs/tree-sitter-javascript/latest/tree_sitter_javascript/>
- TypeScript/TSX grammar crate: <https://docs.rs/tree-sitter-typescript/latest/tree_sitter_typescript/>
- PHP grammar crate: <https://docs.rs/tree-sitter-php/latest/tree_sitter_php/>
- Blade grammar: <https://github.com/EmranMR/tree-sitter-blade>

## Project status

xgraph runs end-to-end. `xgraph init` indexes a worktree into Cozo with
cross-file edges resolved; `xgraph daemon start` opens a Unix socket and
serves in-memory hot indexes loaded from the persistent graph; `xgraph mcp`
lazy-spawns the daemon and proxies MCP-style JSON-RPC; the filesystem
watcher reconciles incremental changes, including ignore-file edits. A
performance pass landed thread-local parser caches, hot-index integration,
content-hash skip caching, and a parallel rayon-backed scanner.

What's deferred (called out in `IMPLEMENTATION_GUIDE.md`'s "Status"
section):

- Incremental Tree-sitter parsing (`old_tree.edit + parse(new, Some(&old))`)
  — full-file parse is always used today. Needs a memory-vs-throughput
  benchmark before being worth the retention cost.
- Laravel resolver wiring. `src/laravel.rs` is implemented and tested but
  not yet invoked by `WorktreeOwner`; it requires the PHP extractor to
  preserve structured call-argument data that the canonical
  `ExtractedFile` doesn't carry today.
- Performance benchmarks (Criterion harness, tracked baselines) — a
  separate scaffolded effort.

See [`AGENTS.md`](./AGENTS.md) for engineering rules and
[`IMPLEMENTATION_GUIDE.md`](./IMPLEMENTATION_GUIDE.md) for the full phase
checklist.

For local verification, run:

```bash
just check
```

If `just` is not installed, run the underlying commands directly:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```
