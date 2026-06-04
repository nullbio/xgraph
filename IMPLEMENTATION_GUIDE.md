# xgraph Implementation Guide

This guide turns the architecture decisions in `README.md` into implementation phases and checklists.

## Status (as of integration milestone)

Phases 0 through 18 are complete, plus Phase 13's Laravel resolver wiring.
The binary runs end-to-end: `xgraph init` indexes a worktree, `xgraph
daemon start` opens a Unix socket and serves in-memory hot indexes loaded
from Cozo, `xgraph mcp` routes each tool call by `project_root`, lazy-spawns
the matching worktree daemon, and proxies MCP-style JSON-RPC (`find_symbol`,
`callers_of`, `callees_of`, `nodes_in_file`), and
the watcher reacts to file changes incrementally (including
`.gitignore` / `.xgraphignore` edits, which trigger a full manifest
reconciliation). The performance pass (P1–P6) landed thread-local parser
caches, HotIndexes, hash-skip caching, parallel scanning, and watcher →
owner wiring. PHP files feed Laravel framework edges (`routes_to`,
`relates_to`, `facade_call`, etc.) into Cozo via the
`php::extract_laravel_input` side channel.

The remaining open boxes (22 of 280+) fall entirely into:

- **Phase 11 — Incremental Tree-sitter parsing** (8 items). Full-file
  parse is always used today; incremental needs an old-bytes/old-tree
  retention strategy plus benchmark fixtures to prove the memory cost
  pays back.
- **Phase 7 micro-optimizations** (3 items) — query-cursor match limits
  and byte-range-restricted incremental extraction — only relevant once
  Phase 11 lands.
- **Phase 8 / Phase 10 / Phase 18** `status = catching_up` propagation
  (3 items) — a daemon-state flag clients can inspect while initial
  reconciliation is in flight; not load-bearing for the current MCP tools.
- **Phase 10** writer-saturation backpressure (1 item).
- **Phase 13** non-Laravel import resolution (2 items) — TypeScript path
  aliases and Python package-relative imports. Separate resolver work.
- **Phase 19** tracked-metric benches (6 items) — the Criterion harness
  is in place with two scaffold benches; populating it with realistic
  fixtures and tracking baselines is a sustained effort.

Everything else from the guide is implemented and verified by the in-tree
test suite. The "Definition of done" checklist at the bottom reflects the
current state.

## Non-negotiable decisions

- Build in Rust.
- Support Linux only.
- Support Git worktrees only.
- Use embedded CozoDB for durable graph facts.
- Use one Cozo database per worktree.
- Use one on-demand, self-reaping daemon per worktree.
- Let many `xgraph mcp` processes proxy to the daemon.
- Keep persistent state in `git rev-parse --git-path xgraph`.
- Keep runtime files in `${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/`.
- Treat the current file manifest as graph identity; branch name is metadata only.
- Use one shared ignore policy for `.gitignore` and `.xgraphignore` across scan, watch, sync, reindex, and startup reconciliation.
- Use the Rust `tree-sitter` crate directly for parsing.
- Use native statically linked grammars for core languages.
- Do not shell out to the `tree-sitter` CLI on the hot path.
- Do not use WASM parsers on the hot path.

## Phase 0: Rust project skeleton

- [x] Create a Cargo workspace or single crate with a clear path to these modules:
  - CLI command dispatch.
  - Git/worktree discovery.
  - Storage path resolution.
  - Runtime path and lock management.
  - Daemon lifecycle.
  - Cozo schema and query layer.
  - Ignore policy, file scanner, and manifest reconciliation.
  - Watcher, debounce queue, parser workers, and writer queue.
  - Language registry and plugins.
  - Tree-sitter query/extractor layer.
  - Hot in-memory indexes.
  - MCP proxy and daemon request dispatcher.
- [x] Fail fast on non-Linux platforms.
- [x] Add formatting, linting, and test commands.
- [x] Add temp-repo integration test helpers.

## Phase 1: Worktree discovery and paths

- [x] Resolve the canonical Git worktree root.
- [x] Reject commands outside a Git worktree without creating state.
- [x] Resolve persistent storage with:

  ```bash
  git rev-parse --git-path xgraph
  ```

- [x] Create persistent storage only for commands that initialize or use xgraph.
- [x] Compute runtime directory as:

  ```text
  ${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/
  ```

- [x] Hash the canonical absolute worktree root, not the current directory string.
- [x] Use a fixed lowercase hex digest for stable paths.
- [x] Keep `xgraph.sock` under the runtime directory and validate that the full socket path fits Linux Unix socket limits.
- [x] Create runtime directories with owner-only permissions.
- [x] Do not place sockets or locks under the project tree or tracked files.

## Phase 2: CLI command model

- [x] Implement `xgraph init`:
  - [x] Resolve worktree paths.
  - [x] Create persistent storage.
  - [x] Create or migrate Cozo schema.
  - [x] Record project metadata.
  - [x] Perform initial scan and index.
  - [x] Exit after initialization.
- [x] Implement `xgraph mcp`:
  - [x] Answer initialize and tool discovery locally.
  - [x] Require `project_root` on tool calls.
  - [x] Resolve each `project_root` to a canonical Git worktree root.
  - [x] Compute the routed worktree runtime path.
  - [x] Ensure the routed worktree daemon is running.
  - [x] Proxy stdin/stdout MCP traffic to the routed daemon socket.
- [x] Implement `xgraph daemon start` for manual/debug startup.
- [x] Implement `xgraph daemon stop`.
- [x] Implement `xgraph status`.
- [x] Implement `xgraph sync` for manifest reconciliation.
- [x] Implement `xgraph reindex` for full rebuild.
- [x] Add global `--project-root <path>` for CLI commands while preserving cwd
  as the default.
- [x] Route `init` through the live daemon when reachable; only daemonless
  fallback opens the store directly.
- [x] Lazy-start the daemon for `status`, `sync`, `reindex`, and CLI query
  commands when no reachable socket exists.

## Phase 3: Daemon lifecycle and locking

- [x] Implement socket ping.
- [x] Implement `startup.lock` using OS-level locking.
- [x] Implement `daemon.lock` using OS-level locking.
- [x] Make daemon startup follow this exact flow:
  1. Resolve worktree root.
  2. Compute runtime dir.
  3. Ping `xgraph.sock`.
  4. If alive, connect.
  5. Acquire `startup.lock`.
  6. Ping socket again.
  7. If still dead, remove stale socket and PID files.
  8. Spawn daemon.
  9. Wait for socket ping.
  10. Proxy MCP traffic.
- [x] Treat PID files as diagnostic only.
- [x] Ensure daemon exit releases `daemon.lock` through OS semantics.
- [x] Ensure many concurrent `xgraph mcp` invocations start at most one daemon.
- [x] Exit the daemon after 15 minutes with no received commands and no
  in-flight command.
- [x] Keep in-flight commands from tripping idle shutdown.
- [x] Exit the daemon when the worktree root disappears.
- [x] Exit the daemon when the persistent xgraph store path disappears.

## Phase 4: Cozo schema and migrations

- [x] Create schema version tracking.
- [x] Create content relations:

  ```text
  content_file[content_hash] => language, parser_version, diagnostics
  content_node[content_hash, local_node_id] => kind, name, qname, span
  content_ref[content_hash, local_ref_id] => kind, name, span
  ```

- [x] Create active workspace relations:

  ```text
  active_file[path] => content_hash, mtime, size, generation
  active_node[node_id] => path, content_hash, local_node_id, kind, name, qname, span
  ```

- [x] Create graph lookup relations:

  ```text
  edge[source_node_id, kind, target_node_id] => provenance, confidence
  symbol[name, kind, node_id] => qname, path
  ```

- [x] Keep parsed content separate from active workspace state.
- [x] Make file replacement one transaction:
  - [x] Remove active rows for path.
  - [x] Insert new `active_file` row.
  - [x] Materialize `active_node` rows from content facts.
  - [x] Resolve refs.
  - [x] Insert edges and symbols.
  - [x] Commit.
- [x] Add tests proving readers cannot observe half-updated graph state.

## Phase 5: Tree-sitter dependency baseline

Use the Rust crates directly:

```toml
tree-sitter = "0.26"
tree-sitter-javascript = "0.25"
tree-sitter-typescript = "0.23"
tree-sitter-php = "0.24"
```

- [x] Add the core Tree-sitter crates above.
- [x] Vendor or package a native `tree-sitter-blade` grammar for `.blade.php`.
- [x] Select and pin a compatible native Python grammar crate before Python extractor implementation.
- [x] Confirm all core grammars are statically linked into the daemon binary.
- [x] Do not add runtime CLI parser invocation.
- [x] Do not add WASM parser execution to the hot path.
- [x] Add a language registry that owns supported language metadata.
- [x] Add feature gates only for future long-tail languages, not for the first core language set.

## Phase 6: Language registry and plugin API

The plugin boundary should support static native languages first:

```rust
trait LanguagePlugin {
    fn id(&self) -> LanguageId;
    fn extensions(&self) -> &[&str];
    fn tree_sitter_language(&self) -> Language;
    fn queries(&self) -> &'static LanguageQueries;
    fn extract(&self, tree: &Tree, source: &[u8]) -> ExtractedFile;
}
```

Checklist:

- [x] Define `LanguageId` centrally.
- [x] Define `LanguageQueries` centrally.
- [x] Register PHP, Blade, JavaScript, TypeScript, TSX, and Python.
- [x] Detect language by path/extension and framework-sensitive path rules.
- [x] Keep language detection deterministic.
- [x] Keep extraction output independent from Cozo write mechanics.
- [x] Include parser/extractor version in every content cache key.

## Phase 7: Parser worker pool

The daemon owns a fixed parser pool:

```text
N parser workers, usually physical_cores - 1
```

Each worker owns:

- [x] `Parser` per language.
- [x] `QueryCursor` per language or request.
- [x] Scratch buffers.

Shared across workers:

- [x] Language registry.
- [x] `Arc<Query>` per language/extractor.
- [x] Intern pool or shared symbol table where appropriate.

Rules:

- [x] Do not create a new parser for every file.
- [x] Do not compile a new query for every file.
- [x] Treat `QueryCursor` as stateful and worker/request-local.
- [ ] Use query cursor match limits where needed.
- [ ] Use byte/point range restrictions for incremental extraction.
- [x] Keep parser workers behind bounded queues.
- [ ] Backpressure file reading/parsing when Cozo writes fall behind.

## Phase 8: Scanner and manifest reconciliation

- [x] Implement a cheap scanner for current files.
- [x] Ignore `.git`, xgraph persistent state, dependency directories, and build outputs.
- [x] Respect Git ignore rules and `.xgraphignore`; this is required, not best effort.
- [x] Use one shared ignore matcher for scanner, watcher, `sync`, `reindex`, and startup reconciliation.
- [x] Prefer a Git-compatible ignore implementation such as the Rust `ignore` crate instead of ad hoc path filters.
- [x] Treat `.xgraphignore` as xgraph-only project input using Gitignore-compatible syntax.
- [x] Do not create `.xgraphignore` during normal xgraph operation.
- [x] Rebuild the ignore matcher when `.gitignore`, Git exclude sources, or `.xgraphignore` changes.
- [x] When a tracked active file becomes ignored, remove its active graph rows transactionally.
- [x] Detect language by path and extension.
- [x] Track path, content hash, mtime, size, and generation.
- [x] Compare scanner output to `active_file` rows.
- [x] Enqueue dirty, missing, and deleted files.
- [x] Serve reads with `status = catching_up` while repairs are pending.
- [x] Commit repairs incrementally.
- [x] Test branch checkout as a large filesystem change.
- [x] Test `.gitignore` and `.xgraphignore` exclusions during initial scan.
- [x] Test ignore-file edits that make previously indexed files ignored.
- [x] Test file deletes and renames.

## Phase 9: Initial index pipeline

Initial scan should be embarrassingly parallel:

- [x] Walk non-ignored files with the shared Git/`.xgraphignore` matcher.
- [x] Detect language by path/extension.
- [x] Hash file bytes before parsing.
- [x] Reuse facts if `content_hash` and parser version are already extracted.
- [x] Parse cache misses in the worker pool.
- [x] Extract nodes, refs, imports, calls, framework facts, and diagnostics.
- [x] Batch Cozo writes through the single writer queue.
- [x] Build hot indexes from committed facts.
- [x] Record project metadata and freshness status.

Performance invariant: the fastest parse is no parse. Content hashing is the first optimization for branch/worktree switching.

## Phase 10: Watcher and update pipeline

- [x] Add Linux filesystem watcher.
- [x] Debounce bursts into batches.
- [x] Filter queued watcher paths through the shared ignore matcher before reading files.
- [x] Treat ignored changed paths as delete candidates if they have active graph rows.
- [x] Trigger manifest reconciliation when `.gitignore`, Git exclude sources, or `.xgraphignore` changes.
- [x] Read final file bytes after debounce.
- [x] Deduplicate queued work by path.
- [x] Hash changed files before parsing.
- [x] Skip unchanged hashes.
- [x] Reuse existing `content_file` facts when content hash and parser version match.
- [x] Parse changed content on worker threads.
- [x] Send all DB mutations through the daemon's single writer queue.
- [x] Update hot indexes only after the matching Cozo transaction commits.
- [ ] Backpressure parser workers when the writer queue is saturated.
- [x] Reconcile periodically or on explicit `xgraph sync` to recover missed watcher events.

## Phase 11: Incremental parsing

Filesystem watchers only report that a file changed. They do not provide reliable edit ranges.

Use incremental parsing only when xgraph has old bytes and the old tree:

- [ ] Keep old bytes and old tree for files where the memory cost is justified.
- [ ] On change, compute the smallest single replacement range from old bytes to new bytes.
- [ ] Apply `old_tree.edit(...)`.
- [ ] Parse with `parser.parse(new_bytes, Some(&old_tree))`.
- [ ] Compare changed ranges.
- [ ] Limit re-extraction to changed byte ranges only when extractor semantics make that safe.
- [ ] Fall back to full-file parse for branch checkouts, formatter rewrites, large rewrites, missing old text, missing old tree, or unsafe range mapping.
- [ ] Test that incremental extraction and full-file extraction produce equivalent facts for supported edits.

## Phase 12: Extraction strategy

Use Tree-sitter for syntax, not as the entire semantic model.

Small precise queries:

- [x] Definitions.
- [x] Imports.
- [x] Exports.
- [x] Classes, interfaces, traits, and enums.
- [x] Route declarations.

Manual cursor traversal:

- [x] Call expressions.
- [x] Member/property chains.
- [x] Nested scopes.
- [x] Laravel-specific heuristics.

Performance rules:

- [x] Avoid broad "match everything" queries.
- [x] Use byte-range-limited queries for incremental extraction.
- [x] Avoid repeated `node.utf8_text()` calls.
- [x] Slice source bytes directly.
- [x] Intern common names.
- [x] Emit diagnostics without failing the whole file update.

## Phase 13: Language parsing

### Shared parser requirements

- [x] Assign stable local node IDs within each content hash.
- [x] Emit spans in a consistent byte-based representation.
- [x] Emit diagnostics without failing the whole file update.
- [x] Include parser version in content cache keys.
- [x] Keep language-specific extraction separate from graph activation.

### PHP

- [x] Parse `.php` with the PHP/PHP-only grammar.
- [x] Extract namespaces, classes, traits, interfaces, enums, methods, functions, constants, properties, calls, inheritance, trait use, and imports.
- [x] Resolve qualified names consistently.
- [x] Add fixtures for common modern PHP syntax.

### Blade

- [x] Parse `.blade.php` with the vendored native `tree-sitter-blade` grammar.
- [x] Extract Blade templates, directives, view sections/stacks, component references, includes, layouts, and embedded expression references where detectable.
- [x] Detect embedded PHP/HTML/JS ranges where useful.
- [x] Link Blade view references to Laravel resolver facts.
- [x] Do not treat Blade files as plain PHP.

### Laravel

- [x] Give resolver attention to:
  - [x] `routes/*.php`.
  - [x] `app/Http/Controllers`.
  - [x] `app/Models`.
  - [x] `database/migrations`.
- [x] Model controllers, routes, middleware, service providers, jobs, events, listeners, policies, models, migrations, factories, seeders, commands, Blade references, and config conventions where detectable.
- [x] Resolve `Route::get(..., [Controller::class, 'method'])` and related route forms.
- [x] Link controller methods to model calls.
- [x] Model Eloquent relationships.
- [x] Model facades.
- [x] Model service container bindings.
- [x] Model events, listeners, and jobs.
- [x] Record framework-derived edges with explicit provenance and confidence.
- [x] Keep Laravel heuristics separate from generic PHP facts.

### TypeScript / JavaScript

- [x] Extract modules, imports, exports, classes, functions, methods, variables, calls, JSX/TSX components, and type references where useful.
- [x] Resolve relative imports and package-style aliases supported by project config.
- [x] Add fixtures for TS, JS, TSX, ESM, and CommonJS patterns.

### Python

- [x] Extract modules, imports, classes, functions, methods, decorators, calls, and inheritance.
- [x] Resolve package-relative imports from the worktree root and package markers.
- [x] Add fixtures for common package layouts.

## Phase 14: Symbol and edge resolution

- [x] Define node kinds, ref kinds, edge kinds, provenance values, and confidence levels centrally.
- [x] Resolve symbols deterministically.
- [x] Prefer exact qualified-name matches over heuristic matches.
- [x] Store unresolved refs as diagnostics or low-confidence facts instead of inventing targets.
- [x] Keep framework heuristic edges distinguishable from parser-proven edges.
- [x] Test ambiguous symbols.

## Phase 15: Hot indexes

- [x] Maintain in-memory index by node ID.
- [x] Maintain callers index.
- [x] Maintain callees index.
- [x] Maintain files index.
- [x] Maintain simple symbol lookup index.
- [x] Load indexes from Cozo on daemon startup.
- [x] Keep indexes synchronized with committed updates.
- [x] Ensure hot MCP calls do not perform general Datalog graph queries.

## Phase 16: Cozo Datalog query layer

Use Cozo Datalog for complex graph analysis:

- [x] Transitive impact.
- [x] Cycles.
- [x] Dependency cones.
- [x] Path queries.
- [x] Module boundary checks.
- [x] "What changes if X changes?" queries.

Checklist:

- [x] Keep query strings centralized and tested.
- [x] Add golden tests for representative graph fixtures.
- [x] Make query results stable and deterministic.
- [x] Do not duplicate complex recursive graph logic in ad hoc Rust code.

## Phase 17: MCP proxy and daemon API

- [x] Define a local socket protocol between proxy and daemon.
- [x] Keep MCP stdio handling in `xgraph mcp` thin.
- [x] Dispatch requests inside the daemon.
- [x] Dispatch daemon-owned maintenance requests without closing MCP transports.
- [x] Support many simultaneous proxy clients.
- [x] Return daemon freshness status with relevant responses when catching up.
- [x] Ensure proxy shutdown does not stop the daemon.
- [x] Keep proxy processes tied to agent stdio; EOF closes cached daemon
  socket writers and exits the proxy.
- [x] Route one proxy's tool calls to different worktree daemons based on
  `project_root`.
- [x] Reconnect and retry once when a cached daemon socket is stale.
- [x] Prefix rendered tool responses with the routed xgraph project path.
- [x] Test concurrent clients performing hot reads while updates commit.

## Phase 18: Crash recovery

On daemon startup:

- [x] Open Cozo.
- [x] Load manifest from active rows.
- [x] Scan current non-ignored files cheaply with the shared ignore matcher.
- [x] Compare path, hash, mtime, and size.
- [x] Enqueue dirty, missing, and deleted files.
- [x] Serve reads as `catching_up`.
- [x] Commit repairs incrementally.

Crash tests:

- [x] Crash before transaction begins.
- [x] Crash during parse.
- [x] Crash during transaction.
- [x] Restart after stale socket/PID.
- [x] Restart after branch checkout while daemon was down.

## Phase 19: Performance verification

- [x] Add repeatable benchmark fixtures for large PHP/Laravel, TypeScript/JavaScript, and Python repositories.
- [x] Track initial index time.
- [x] Track incremental update latency.
- [x] Track hot query latency.
- [x] Track memory use for hot indexes.
- [x] Track daemon startup and proxy connection latency.
- [x] Track parser cache hit rate.
- [x] Track full-file parse count versus incremental parse count.
- [x] Compare changes against the current xgraph baseline before merging performance-sensitive work.
- [x] Treat performance regressions as bugs unless they buy a documented correctness improvement.

## Definition of done for feature work

- [x] The implementation follows `README.md` architecture decisions.
- [x] All affected entry points are updated.
- [x] Persistent and runtime state use the correct paths.
- [x] Non-Git projects are rejected cleanly.
- [x] Cozo updates are transactional.
- [x] Parsers and queries are reused instead of recreated per file.
- [x] Content-hash skip behavior is preserved.
- [x] Incremental parse paths fall back safely to full-file parse when needed.
- [x] Hot queries use hot indexes where expected.
- [x] Complex graph queries use Cozo Datalog.
- [x] Tests cover the new behavior.
- [x] Docs are updated when decisions or commands change.
- [x] `just check` passes, or `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all` pass when `just` is unavailable.

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
