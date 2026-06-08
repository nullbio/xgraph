# AGENTS.md

Project-specific instructions for agents working on xgraph.

## Product direction

xgraph is a Linux-only, Rust-native successor to `codegraph` with these fixed decisions:

- Rust implementation.
- Linux only.
- Embedded CozoDB, not SQLite.
- One database per Git worktree.
- One on-demand, self-reaping daemon per worktree.
- Many lightweight `xgraph mcp` proxy processes connect to that daemon.
- Only Git projects are supported; projects without `.git` are ignored.
- The database represents current files on disk, not a Git branch name.
- File watching, daemon lifecycle, parser scheduling, branch/worktree detection, hot indexes, and agent coordination belong to xgraph, not Cozo.
- Cozo is for durable graph facts, transactions/snapshots, and complex Datalog queries.
- Tree-sitter is used through native Rust crates on the hot path.

## Architecture invariants

Do not violate these without updating `README.md` and `IMPLEMENTATION_GUIDE.md` in the same change:

- Persistent state must live under `git rev-parse --git-path xgraph`.
- Runtime files must live under `${XDG_RUNTIME_DIR:-/tmp}/xgraph/<hash-of-worktree-root>/` to avoid Unix socket length limits.
- Runtime state is disposable; persistent graph/config state is not.
- PID files are diagnostic only. Correct daemon ownership must use OS-level locks.
- `startup.lock` prevents duplicate lazy starts.
- `daemon.lock` is held for the daemon lifetime.
- Daemons must exit after 15 minutes with no received commands and no in-flight command.
- Daemons must exit when their worktree root or persistent xgraph store path disappears.
- There must be exactly one writer queue per worktree daemon.
- There must not be multiple watchers or parser pools for the same worktree.
- MCP proxy processes must not parse source files or write Cozo directly.
- A file update must replace active rows transactionally.
- Readers must see either the old graph or the new graph, never a half-updated graph.
- Startup must reconcile the manifest with current files before reporting the graph as fully fresh.
- Branch checkout is handled as filesystem churn; branch name is metadata only.
- New Git worktrees get isolated databases.
- Scanner, watcher, `sync`, `reindex`, and startup reconciliation must all use the same ignore matcher.
- The ignore matcher must honor Git ignore rules and `.xgraphignore` before reading, hashing, parsing, or writing file facts.
- Changes to `.gitignore`, Git exclude sources, or `.xgraphignore` must trigger manifest reconciliation; newly ignored active files are removed transactionally.

## Parser invariants

- Use the Rust `tree-sitter` crate directly.
- Do not shell out to the `tree-sitter` CLI on the hot path.
- Do not use WASM parsers on the hot path.
- Core grammars must be native and statically linked.
- Initial crate versions are:
  - `tree-sitter = "0.26"`
  - `tree-sitter-go = "0.25"`
  - `tree-sitter-javascript = "0.25"`
  - `tree-sitter-typescript = "0.23"`
  - `tree-sitter-php = "0.24"`
  - `tree-sitter-rust = "0.24"`
- Use a vendored native `tree-sitter-blade` grammar for `.blade.php` files.
- Python parser support must also use a selected/pinned native grammar crate.
- Compile Tree-sitter queries once and share them as `Arc<Query>`.
- Keep `QueryCursor` instances per worker/request because they are stateful.
- Do not create a new parser or compile queries for every file.
- Keep old bytes and old trees only when they enable correct incremental parsing.
- Incremental parsing requires old bytes plus old tree; filesystem watcher events alone are not enough.
- Use full-file parsing for branch checkouts, formatter rewrites, or any case where a safe edit range is not available.

## Performance Guidelines

Performance is a primary design constraint for xgraph. Prefer simple, measured hot paths over general abstractions.

- Keep the hot path in memory whenever possible. Common MCP queries like symbol lookup, callers, callees, files, and shallow impact should use daemon-owned indexes, not raw Cozo queries, unless benchmarks show otherwise.
- Treat CozoDB as the durable graph and complex-query engine. Do not route simple lookups through Datalog when a direct map/index lookup is available.
- Avoid duplicate work across agents. There must be one watcher, one parser pool, one write queue, and one Cozo connection owner per worktree.
- Hash before parsing. If file content has not changed, do not parse, extract, resolve, or write.
- Batch filesystem events. Debounce short bursts, coalesce repeated changes to the same path, and commit file updates in bounded batches.
- Prefer full-file parse plus content-hash skipping first. Add Tree-sitter incremental parsing only when benchmarks show parsing is the bottleneck.
- Reuse expensive objects: Tree-sitter parsers, compiled queries, query cursors, buffers, string interning tables, database handles, prepared queries, and worker pools.
- Keep write ownership simple. All DB writes go through the daemon's single writer queue. Readers should not be blocked by parsing or write scheduling.
- Make startup recovery cheap and correct. On daemon start, reconcile manifests using file metadata and hashes; do not blindly reindex everything.
- Design IDs and indexes for lookup speed. Prefer stable, deterministic keys and prefix-friendly indexes over random row IDs that require extra joins.
- Respect ignore rules aggressively. Never scan `.git`, `.xgraph`, `node_modules`, `vendor`, `dist`, `build`, or generated output unless explicitly configured.
- Avoid filesystem work in query handlers. Queries should operate on already-indexed data and in-memory structures, not scan or stat the project tree.
- Avoid spawning processes on hot paths. No shelling out to `git`, `tree-sitter`, package managers, or language servers during normal query handling.

## Language priorities

Language support is limited to:

1. PHP.
2. Laravel framework conventions, including Blade.
3. TypeScript / JavaScript.
4. Python.
5. Go.
6. Rust.

Do not broaden language scope further before these languages handle definitions,
imports, calls, and path-aware cross-file resolution well.

## Extraction rules

- Tree-sitter gives syntax; xgraph extractor/resolver passes produce code meaning.
- Prefer small precise queries for definitions, imports, exports, classes/interfaces/traits, and route declarations.
- Prefer manual cursor traversal for call expressions, member/property chains, nested scopes, and Laravel heuristics.
- Keep Laravel heuristics separate from generic PHP facts.
- Store framework-derived edges with explicit provenance and confidence.
- Store unresolved refs as unresolved or diagnostic facts; do not invent targets.
- Keep language-specific extraction separate from active graph materialization.

## Implementation style

- Read existing code before changing it.
- Prefer the proper fix over compatibility shims or workarounds.
- Do not add fallback storage locations unless explicitly requested.
- Do not create tracked project files for xgraph state.
- Do not implement ignore behavior with ad hoc path checks when a Git-compatible ignore parser is available.
- Keep abstractions opaque: callers should not depend on internal tuple/struct shapes when operations would be cleaner.
- Remove dead code instead of keeping unused compatibility paths.
- Do not add comments unless the logic is genuinely non-obvious.
- When a source of truth moves, update all entry points that now depend on it.
- If a decision changes, update all docs that encode that decision.

## Testing expectations

Use the repo's actual command:

```bash
just check
```

If `just` is not installed, run the underlying commands directly:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Expected test coverage:

- Unit tests for path resolution, hashing, manifest comparison, schema helpers, parser utilities, query builders, diff ranges, and language registry behavior.
- Integration tests using temporary Git repositories for `init`, `mcp` lazy startup, daemon locks, branch checkout reconciliation, deletion handling, and crash recovery.
- Concurrency tests for many MCP proxies sharing one daemon.
- Lifecycle tests for idle timeout, in-flight command protection, deleted worktrees, stale sockets, and reconnect after daemon restart.
- Parser fixtures for PHP, Blade/Laravel, TypeScript/JavaScript, Python, Go, and Rust.
- Incremental parsing tests that compare full parse extraction with incremental extraction for the same edit.

Docs-only changes do not require Rust commands, but generated documentation should still be reviewed for consistency with the architecture invariants above.
