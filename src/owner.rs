//! Per-worktree owner that bundles the daemon's long-lived dependencies.
//!
//! Owns the Cozo store, writer queue, ignore matcher, language registry, and
//! the parser-version constant used in content fact rows. Provides the
//! daemon-side primitives for initial indexing and incremental updates.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cozo::{
    ContentHash as CozoContentHash, CozoStore, EdgeFact, FileUpdate, FileUpdateMetadata,
    WriterError, WriterHandle, WriterQueue, active_node_id,
};
use crate::daemon_status::DaemonStatus;
use crate::extract::ExtractedFile;
use crate::hash::ContentHash;
use crate::ignore::IgnoreMatcher;
use crate::import_resolver::{PythonImportResolver, TsAliasResolver};
use crate::indexes::HotIndexes;
use crate::language::LanguageRegistry;
use crate::scanner::{DetectedLanguage, ScanError, scan};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rayon::prelude::*;

pub const PARSER_VERSION: u32 = 1;

/// Outcome of a full index pass. Includes the per-pass counts that
/// codegraph's `SyncResult` exposes so `init_at` can print a summary
/// without re-querying Cozo, plus a phase-level wall-clock breakdown
/// the benchmarks read to identify which phase dominates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexSummary {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub nodes_created: u64,
    pub edges_created: u64,
    pub timings: PhaseTimings,
}

/// Wall-clock duration of each phase of `index_all_with_progress`, in
/// microseconds. Reported even when no `Progress` renderer is active so
/// benchmarks and callers can attribute hot-path costs without
/// re-running the indexer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseTimings {
    pub scan_us: u64,
    pub parse_us: u64,
    pub resolve_us: u64,
    pub store_us: u64,
}

pub type MaintenanceResult = Result<IndexSummary, OwnerError>;
pub type MaintenanceSender = Sender<MaintenanceCommand>;
pub type MaintenanceReceiver = Receiver<MaintenanceCommand>;

#[derive(Debug)]
pub enum MaintenanceCommand {
    Sync { reply: Sender<MaintenanceResult> },
    Reindex { reply: Sender<MaintenanceResult> },
}

impl MaintenanceCommand {
    pub fn sync() -> (Self, Receiver<MaintenanceResult>) {
        let (reply, rx) = crossbeam_channel::bounded(1);
        (Self::Sync { reply }, rx)
    }

    pub fn reindex() -> (Self, Receiver<MaintenanceResult>) {
        let (reply, rx) = crossbeam_channel::bounded(1);
        (Self::Reindex { reply }, rx)
    }
}

pub fn maintenance_channel() -> (MaintenanceSender, MaintenanceReceiver) {
    unbounded()
}

pub struct WorktreeOwner {
    worktree_root: PathBuf,
    matcher: IgnoreMatcher,
    registry: LanguageRegistry,
    store: CozoStore,
    writer: WriterHandle,
    indexes: Arc<HotIndexes>,
    status: Arc<DaemonStatus>,
    generation: u64,
}

impl WorktreeOwner {
    /// Build an owner against a freshly-opened Cozo store. Caller supplies
    /// the store, hot indexes, and shared daemon status. The owner mirrors
    /// every accepted `FileUpdate` into the indexes and tracks which paths
    /// are mid-flight so MCP handlers can attach a precise `catching_up`
    /// flag to file-scoped queries.
    pub fn new(
        worktree_root: PathBuf,
        matcher: IgnoreMatcher,
        registry: LanguageRegistry,
        store: CozoStore,
        indexes: Arc<HotIndexes>,
        status: Arc<DaemonStatus>,
    ) -> Result<Self, crate::cozo::CozoError> {
        // Keep a read-side handle for the hash-skip cache; the writer thread
        // owns its own clone of the same underlying DbInstance.
        let writer = WriterQueue::start(store.clone())?;
        Ok(Self {
            worktree_root,
            matcher,
            registry,
            store,
            writer,
            indexes,
            status,
            generation: 1,
        })
    }

    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// Walk the worktree, extract every supported file, resolve refs into edges
    /// against the full set, then submit one `FileUpdate` per file.
    pub fn index_all(&mut self) -> Result<IndexSummary, OwnerError> {
        self.index_all_with_progress(&crate::progress::Progress::start())
    }

    /// Same as `index_all` but reports progress updates to the caller-provided
    /// renderer. Phase boundaries: scan → parse → resolve → store.
    pub fn index_all_with_progress(
        &mut self,
        progress: &crate::progress::Progress,
    ) -> Result<IndexSummary, OwnerError> {
        use crate::progress::Phase;
        use std::time::Instant;

        progress.phase(Phase::Scanning, None);
        let t_scan_start = Instant::now();
        let scanned = scan(&self.worktree_root, &self.matcher)?;
        let files_scanned = scanned.len();
        let scan_us = t_scan_start.elapsed().as_micros() as u64;
        progress.tick(files_scanned as u64);
        progress.finish_phase();

        // Project-scoped import resolvers, built once per pass.
        let ts_resolver = TsAliasResolver::from_worktree(&self.worktree_root);
        let py_resolver = PythonImportResolver::from_worktree(&self.worktree_root);

        progress.phase(Phase::Parsing, Some(scanned.len() as u64));
        let t_parse_start = Instant::now();
        // Rayon-parallelize file extraction. Each worker thread owns its
        // own tree-sitter `Parser` and `QueryCursor` via the language
        // modules' `thread_local!` slots, so there's no cross-thread
        // parser contention. `prepare_file` takes `&self` and only
        // touches `Sync` fields (registry, store, worktree_root), so
        // concurrent calls are safe.
        let progress_tick = std::sync::atomic::AtomicU64::new(0);
        let prepared: Vec<PreparedFile> = scanned
            .into_par_iter()
            .map(|file| -> Result<Option<PreparedFile>, OwnerError> {
                // Tick on entry so the bar reflects work started, not
                // work finished; the difference is invisible for fast
                // files but matters when a single file stalls on I/O.
                let i = progress_tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                progress.tick(i);
                let Some(lang) = file.language else {
                    return Ok(None);
                };
                let Some(mut prep) = self.prepare_file(file.path, file.mtime, file.size, lang)?
                else {
                    return Ok(None);
                };
                rewrite_imports(
                    &mut prep.extracted,
                    &prep.relative,
                    lang,
                    &ts_resolver,
                    &py_resolver,
                    &self.worktree_root,
                );
                Ok(Some(prep))
            })
            .collect::<Result<Vec<Option<PreparedFile>>, OwnerError>>()?
            .into_iter()
            .flatten()
            .collect();
        let parse_us = t_parse_start.elapsed().as_micros() as u64;
        progress.finish_phase();

        progress.phase(Phase::Resolving, Some(prepared.len() as u64));
        let t_resolve_start = Instant::now();
        let symbol_table = build_symbol_table(&prepared);
        let resolve_us = t_resolve_start.elapsed().as_micros() as u64;
        progress.tick(prepared.len() as u64);
        progress.finish_phase();

        progress.phase(Phase::Storing, Some(prepared.len() as u64));
        let t_store_start = Instant::now();
        let files_indexed = prepared.len();
        let mut nodes_created: u64 = 0;
        let mut edges_created: u64 = 0;
        for (i, prep) in prepared.into_iter().enumerate() {
            nodes_created += prep.extracted.nodes.len() as u64;
            let edge_count = self.submit_prepared(prep, &symbol_table)?;
            edges_created += edge_count as u64;
            progress.tick(i as u64 + 1);
        }
        // Block until the writer thread commits every batch we just
        // submitted. Without this, `store_us` would only measure the
        // (very fast) channel-send time and miss the actual Cozo
        // transaction work that runs asynchronously.
        self.writer.flush()?;
        let store_us = t_store_start.elapsed().as_micros() as u64;
        progress.finish_phase();
        // After the initial walk completes, MCP queries no longer need the
        // "still booting" caveat. Incremental change-driven catching_up is
        // tracked per-path via DaemonStatus::pending_paths.
        self.status.mark_reconcile_done();
        Ok(IndexSummary {
            files_scanned,
            files_indexed,
            nodes_created,
            edges_created,
            timings: PhaseTimings {
                scan_us,
                parse_us,
                resolve_us,
                store_us,
            },
        })
    }

    pub fn sync_all_with_progress(
        &mut self,
        progress: &crate::progress::Progress,
    ) -> Result<IndexSummary, OwnerError> {
        self.status.mark_reconcile_running();
        self.index_all_with_progress(progress)
    }

    pub fn reindex_all_with_progress(
        &mut self,
        progress: &crate::progress::Progress,
    ) -> Result<IndexSummary, OwnerError> {
        self.status.mark_reconcile_running();
        self.writer.flush()?;
        self.writer.truncate_graph()?;
        self.indexes.clear();
        self.generation = 1;
        self.index_all_with_progress(progress)
    }

    /// Re-extract a single path and submit the update. Cross-file resolution
    /// uses an empty symbol table (callers will be resolved on the next
    /// scheduled reconciliation pass).
    pub fn process_change(&mut self, path: PathBuf) -> Result<bool, OwnerError> {
        if !path.exists() {
            return Ok(false);
        }
        // Relative path for pending-set tracking; matches the format MCP
        // handlers will query with.
        let relative = path
            .strip_prefix(&self.worktree_root)
            .unwrap_or(&path)
            .to_path_buf();
        self.status.mark_pending(&relative);
        let result = self.process_change_inner(path);
        self.status.unmark_pending(&relative);
        result
    }

    fn process_change_inner(&mut self, path: PathBuf) -> Result<bool, OwnerError> {
        let metadata = fs::metadata(&path).map_err(|source| OwnerError::Io {
            path: path.clone(),
            source,
        })?;
        let mtime = metadata.modified().unwrap_or_else(|_| SystemTime::now());
        let size = metadata.len();
        let Some(lang) = crate::scanner::detect_language(&path) else {
            return Ok(false);
        };
        let Some(prep) = self.prepare_file(path, mtime, size, lang)? else {
            return Ok(false);
        };
        let empty = SymbolTable::default();
        let _ = self.submit_prepared(prep, &empty)?;
        Ok(true)
    }

    fn prepare_file(
        &self,
        path: PathBuf,
        mtime: SystemTime,
        size: u64,
        language: DetectedLanguage,
    ) -> Result<Option<PreparedFile>, OwnerError> {
        let bytes = fs::read(&path).map_err(|source| OwnerError::Io {
            path: path.clone(),
            source,
        })?;
        let content_hash = crate::hash::hash_bytes(&bytes);
        let relative = path
            .strip_prefix(&self.worktree_root)
            .unwrap_or(&path)
            .to_path_buf();

        // Hash-skip cache: if Cozo already has this exact hash for this path,
        // skip extraction. The fastest parse is no parse.
        let relative_str = relative.to_string_lossy();
        if let Ok(Some(prior)) = self.store.active_file_hash(&relative_str)
            && prior.as_bytes() == content_hash.as_bytes()
        {
            return Ok(None);
        }

        let Some(extracted) = self.registry.extract_file(&relative, &bytes) else {
            return Ok(None);
        };
        let source_bytes = if matches!(language, DetectedLanguage::Php) {
            Some(bytes)
        } else {
            None
        };
        Ok(Some(PreparedFile {
            relative,
            content_hash,
            language,
            mtime,
            size,
            extracted,
            source_bytes,
        }))
    }

    fn submit_prepared(
        &mut self,
        prep: PreparedFile,
        symbols: &SymbolTable,
    ) -> Result<usize, OwnerError> {
        let cozo_hash = CozoContentHash::from_bytes(*prep.content_hash.as_bytes());
        let mut edges = resolve_edges(
            &prep.content_hash,
            &prep.extracted,
            symbols,
            language_family(prep.language),
        );

        // Laravel-specific framework edges run as a post-pass on PHP files.
        // The PHP plugin produces a structured `PhpExtractInput` (with call
        // receivers, methods, and argument literals) that the canonical
        // `ExtractedFile` does not preserve; the resolver consumes it and
        // emits edges with `Provenance::LaravelHeuristic`.
        //
        // Synthetic node IDs use the `lh:` prefix so they cannot collide
        // with parser-extracted IDs (which always start with the 64-hex-char
        // content hash). MCP clients reading these edges should treat
        // `lh:*` IDs as framework synthesis points, not as nodes that
        // exist in `active_node`.
        if matches!(prep.language, DetectedLanguage::Php)
            && let Some(bytes) = &prep.source_bytes
            && let Some(laravel_input) =
                crate::languages::php::extract_laravel_input(bytes, &prep.relative)
        {
            let facts = crate::laravel::resolve(std::slice::from_ref(&laravel_input));
            append_framework_edges(&facts, &mut edges);
        }
        // Blade templates feed the same resolver via a separate input shape.
        // The Blade ref kinds (`blade_view`, `blade_component`,
        // `blade_x_component`) are translated 1:1 to `BladeRef`s. The
        // resolver synthesizes `view.<dotted>` source IDs from the template
        // path, so every Blade ref produces a framework edge whose source
        // is the template itself and target is the referenced view or
        // component.
        if matches!(prep.language, DetectedLanguage::Blade) {
            let blade_input = blade_input_from_extracted(&prep.relative, &prep.extracted);
            if !blade_input.refs.is_empty() {
                let facts = crate::laravel::resolve_blade(std::slice::from_ref(&blade_input));
                append_framework_edges(&facts, &mut edges);
            }
        }
        // React resolver runs on every JS/TS/TSX file. The resolver is
        // syntactic only — it inspects the already-extracted nodes/refs,
        // so this is a constant-time pass per file with no re-parsing.
        if matches!(
            prep.language,
            DetectedLanguage::JavaScript | DetectedLanguage::TypeScript | DetectedLanguage::Tsx
        ) {
            let facts = crate::react::resolve_react(&[&prep.extracted]);
            append_framework_edges(&facts, &mut edges);
        }

        let metadata = FileUpdateMetadata {
            content_hash: cozo_hash,
            language: language_label(prep.language).to_owned(),
            parser_version: PARSER_VERSION,
            mtime: mtime_seconds(prep.mtime),
            size: prep.size,
            generation: self.generation,
        };
        self.generation += 1;

        let mut update = FileUpdate::from_extracted(&prep.extracted, metadata);
        update.path = prep.relative.to_string_lossy().into_owned();
        update.edges = edges;
        // Mirror the update into the hot indexes so MCP reads see the new state
        // immediately; the writer thread persists the same facts to Cozo.
        self.indexes.apply_file_update(&update);
        let edge_count = update.edges.len();
        self.writer.submit(update)?;
        Ok(edge_count)
    }

    /// Rebuild the ignore matcher after a `.gitignore` / `.xgraphignore`
    /// change, then reconcile the active manifest: previously-indexed paths
    /// that are now ignored (or missing) get a delete; new or changed paths
    /// get an extract.
    ///
    /// Cheap for unchanged files because of the hash-skip cache.
    ///
    /// **Recovery semantics:** if `index_all` fails partway through, the
    /// new matcher has already been swapped in but only some files have
    /// been re-indexed. A subsequent retry continues from where it left off
    /// because the hash-skip cache treats already-submitted files as no-ops;
    /// no explicit rollback is needed.
    pub fn reconcile_after_ignore_change(&mut self) -> Result<(), OwnerError> {
        // Swap in a fresh matcher.
        self.matcher = IgnoreMatcher::new(&self.worktree_root).map_err(|err| OwnerError::Io {
            path: self.worktree_root.clone(),
            source: std::io::Error::other(err.to_string()),
        })?;

        // Walk current files under the new matcher and submit anything
        // changed (hash-skip suppresses no-ops).
        let scanned_set: std::collections::HashSet<PathBuf> =
            scan(&self.worktree_root, &self.matcher)?
                .iter()
                .map(|f| {
                    f.path
                        .strip_prefix(&self.worktree_root)
                        .unwrap_or(&f.path)
                        .to_path_buf()
                })
                .collect();
        self.index_all()?;

        // Find paths in `active_file` that no longer pass the new matcher.
        let active_paths: Vec<String> = self
            .store
            .run_read(
                "?[path] := *active_file[path, _hash, _mtime, _size, _gen]",
                std::collections::BTreeMap::new(),
            )?
            .rows
            .into_iter()
            .filter_map(|row| {
                row.into_iter().next().and_then(|v| match v {
                    cozo::DataValue::Str(s) => Some(String::from(s.as_str())),
                    _ => None,
                })
            })
            .collect();
        for rel in active_paths {
            let rel_path = PathBuf::from(&rel);
            if !scanned_set.contains(&rel_path) {
                self.process_delete(self.worktree_root.join(&rel_path))?;
            }
        }
        Ok(())
    }

    /// Drop all facts for a path that no longer exists on disk. Submits an
    /// empty `FileUpdate` so the Cozo transaction removes the active rows;
    /// also clears the hot-index state for the path.
    pub fn process_delete(&mut self, path: PathBuf) -> Result<(), OwnerError> {
        let relative = path
            .strip_prefix(&self.worktree_root)
            .unwrap_or(&path)
            .to_path_buf();
        self.status.mark_pending(&relative);
        let result = self.process_delete_inner(relative.clone());
        self.status.unmark_pending(&relative);
        result
    }

    fn process_delete_inner(&mut self, relative: PathBuf) -> Result<(), OwnerError> {
        let metadata = FileUpdateMetadata {
            content_hash: CozoContentHash::from_bytes([0u8; 32]),
            language: String::new(),
            parser_version: PARSER_VERSION,
            mtime: 0,
            size: 0,
            generation: self.generation,
        };
        self.generation += 1;
        let update = FileUpdate {
            path: relative.to_string_lossy().into_owned(),
            content_hash: metadata.content_hash,
            language: metadata.language,
            parser_version: metadata.parser_version,
            mtime: metadata.mtime,
            size: metadata.size,
            generation: metadata.generation,
            diagnostics: Vec::new(),
            nodes: Vec::new(),
            refs: Vec::new(),
            edges: Vec::new(),
        };
        self.indexes.remove_path(&relative);
        self.writer.submit(update)?;
        Ok(())
    }

    /// Drain pending submissions and return any errors the writer thread recorded.
    pub fn shutdown(mut self) -> Vec<WriterError> {
        self.writer.shutdown();
        self.writer.take_errors()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OwnerError {
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Cozo(#[from] crate::cozo::CozoError),
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error("io error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

struct PreparedFile {
    relative: PathBuf,
    content_hash: ContentHash,
    language: DetectedLanguage,
    mtime: SystemTime,
    size: u64,
    extracted: ExtractedFile,
    /// Source bytes — retained only when a downstream framework resolver
    /// (currently Laravel for PHP) needs to re-walk the tree for structured
    /// call-argument data. `None` for non-PHP languages keeps memory lean.
    source_bytes: Option<Vec<u8>>,
}

/// Group languages that can share symbol references. PHP and Blade are
/// the same ecosystem (Blade calls PHP classes / Laravel facades, etc.);
/// JS / TS / TSX freely cross-reference each other; Python is its own
/// island. Keeping refs from one family from matching a definition in
/// another stops bare-name collisions across unrelated codebases — e.g.
/// PHP's Laravel `config()` helper used to match a JavaScript
/// `config` variable in storybook setup files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LanguageFamily {
    PhpBlade,
    JsTs,
    Python,
}

fn language_family(lang: DetectedLanguage) -> LanguageFamily {
    match lang {
        DetectedLanguage::Php | DetectedLanguage::Blade => LanguageFamily::PhpBlade,
        DetectedLanguage::JavaScript | DetectedLanguage::TypeScript | DetectedLanguage::Tsx => {
            LanguageFamily::JsTs
        }
        DetectedLanguage::Python => LanguageFamily::Python,
    }
}

/// Maps a (language-family, qualified name) pair to the set of global
/// node ids that own it. The language-family key prevents a Python /
/// JS / Blade symbol from being treated as a valid target for a PHP
/// ref (and vice-versa).
#[derive(Default)]
struct SymbolTable {
    by_family_qname: HashMap<(LanguageFamily, String), Vec<String>>,
}

impl SymbolTable {
    fn register(&mut self, family: LanguageFamily, qname: &str, node_id: String) {
        if qname.is_empty() {
            return;
        }
        self.by_family_qname
            .entry((family, qname.to_owned()))
            .or_default()
            .push(node_id);
    }

    fn lookup(&self, family: LanguageFamily, qname: &str) -> &[String] {
        self.by_family_qname
            .get(&(family, qname.to_owned()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn build_symbol_table(prepared: &[PreparedFile]) -> SymbolTable {
    let mut table = SymbolTable::default();
    for prep in prepared {
        let cozo_hash = CozoContentHash::from_bytes(*prep.content_hash.as_bytes());
        let family = language_family(prep.language);
        // Path-scoped import keys are only meaningful for top-level
        // definitions, but every JS/TS definition also gets a file-local key
        // used by call/JSX refs that were not bound to an import.
        let js_path_prefix = match prep.language {
            DetectedLanguage::JavaScript | DetectedLanguage::TypeScript | DetectedLanguage::Tsx => {
                Some(js_path_key(&prep.relative))
            }
            _ => None,
        };
        let python_module_prefix = if matches!(prep.language, DetectedLanguage::Python) {
            python_module_path(&prep.relative)
        } else {
            None
        };

        for node in &prep.extracted.nodes {
            let node_id = active_node_id(&cozo_hash, node.id);
            table.register(family, &node.qname, node_id.clone());
            // Also index by bare name when it differs, so unqualified callers can resolve.
            if node.name != node.qname && should_register_bare_name(family, &node.kind) {
                table.register(family, &node.name, node_id.clone());
            }

            if let Some(prefix) = js_path_prefix.as_deref() {
                table.register(
                    family,
                    &js_local_symbol_key(prefix, &node.name),
                    node_id.clone(),
                );
            }

            // Cross-file linking: top-level defs become importable under a
            // path-scoped composite key. Skip nested defs (methods, inner
            // classes, etc.) — only top-level decls can be `export`ed.
            if node.parent.is_some() {
                continue;
            }
            if let Some(prefix) = js_path_prefix.as_deref() {
                table.register(family, &format!("{prefix}#{}", node.name), node_id.clone());
                // For `index.{ts,tsx,js,jsx}` files, also register under the
                // containing directory so `import X from './utils'` matches
                // when `./utils` is a directory containing `index.ts`.
                if let Some(dir_prefix) = strip_index_suffix(prefix) {
                    table.register(
                        family,
                        &format!("{dir_prefix}#{}", node.name),
                        node_id.clone(),
                    );
                }
            }
            if let Some(module) = python_module_prefix.as_deref() {
                table.register(family, &format!("{module}.{}", node.name), node_id);
            }
        }
    }
    table
}

/// Strip the file extension and convert to forward-slash form so the result
/// composes with `TsAliasResolver`'s output (`src/utils/format`).
fn js_path_key(relative: &Path) -> String {
    let stem = relative.with_extension("");
    stem.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

fn strip_index_suffix(path_key: &str) -> Option<&str> {
    path_key.strip_suffix("/index")
}

fn js_local_symbol_key(path_prefix: &str, name: &str) -> String {
    format!("{path_prefix}#local#{name}")
}

/// Map a Python source path (e.g. `pkg/sub/helper.py`) to its dotted module
/// name (`pkg.sub.helper`). `__init__.py` files use the parent directory.
fn python_module_path(relative: &Path) -> Option<String> {
    let stem = relative.file_stem()?.to_str()?;
    let dir = relative.parent()?;
    let dir_parts: Vec<String> = dir
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .map(str::to_owned)
        .collect();
    let parts = if stem == "__init__" {
        dir_parts
    } else {
        let mut p = dir_parts;
        p.push(stem.to_owned());
        p
    };
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("."))
}

fn resolve_edges(
    hash: &ContentHash,
    extracted: &ExtractedFile,
    symbols: &SymbolTable,
    family: LanguageFamily,
) -> Vec<EdgeFact> {
    let cozo_hash = CozoContentHash::from_bytes(*hash.as_bytes());
    let mut edges = Vec::new();
    for r in &extracted.refs {
        let lookup_key = if family == LanguageFamily::JsTs && is_jsts_precise_ref(&r.kind) {
            let Some(qname) = r.qname.as_deref() else {
                continue;
            };
            if !qname.contains('#') {
                continue;
            }
            qname
        } else {
            r.qname.as_deref().unwrap_or(r.name.as_str())
        };
        if family == LanguageFamily::PhpBlade && is_php_member_call_ref(&r.kind) {
            let Some(qname) = r.qname.as_deref() else {
                continue;
            };
            if !qname.contains("::") {
                continue;
            }
        }
        let targets = symbols.lookup(family, lookup_key);
        if targets.is_empty() {
            continue;
        }
        let edge_kind = edge_kind_for_ref(&r.kind);
        // Refs inside a function/method/class have a concrete container; use
        // the container's active_node_id as the edge source. Refs at module
        // scope (top-level imports especially) have no container; for import
        // edges we synthesize a stable file-level source ID so the edge
        // still emits. For other top-level refs the edge is suppressed to
        // avoid noise from anonymous module code.
        let source_node_id = match r.container {
            Some(local) => active_node_id(&cozo_hash, local),
            None => {
                if is_file_level_edge_kind(edge_kind) {
                    file_level_node_id(&extracted.path)
                } else {
                    continue;
                }
            }
        };
        for target in targets {
            edges.push(EdgeFact {
                source_node_id: source_node_id.clone(),
                kind: edge_kind.to_owned(),
                target_node_id: target.clone(),
                provenance: "parser_extract".to_owned(),
                confidence: 80,
            });
        }
    }
    edges
}

fn should_register_bare_name(family: LanguageFamily, node_kind: &str) -> bool {
    if family != LanguageFamily::PhpBlade {
        return true;
    }
    matches!(
        node_kind,
        "class" | "interface" | "trait" | "enum" | "function"
    )
}

fn is_php_member_call_ref(kind: &str) -> bool {
    matches!(kind, "method_call" | "static_call" | "nullsafe_method_call")
}

fn is_jsts_precise_ref(kind: &str) -> bool {
    matches!(
        kind,
        "call" | "member_call" | "member_access" | "jsx_component"
    )
}

/// File-level synthesized node ID. The `file:` prefix avoids any collision
/// with `active_node_id` (always starts with a 64-hex content hash) and the
/// laravel-heuristic `lh:` prefix. Consumers reading edges with this source
/// know the edge originates from a module-level (file-scope) reference.
fn file_level_node_id(relative: &Path) -> String {
    format!("file:{}", relative.to_string_lossy())
}

fn is_file_level_edge_kind(edge_kind: &str) -> bool {
    matches!(edge_kind, "imports" | "exports")
}

fn edge_kind_for_ref(kind: &str) -> &str {
    match kind {
        "call" | "member_call" | "method_call" | "static_call" | "nullsafe_method_call" => "calls",
        "extends" | "inheritance" => "inherits",
        "implements" => "implements",
        "trait_use" => "uses",
        "import" | "import_esm" | "import_cjs" => "imports",
        "import_named" | "import_default" | "import_namespace" => "imports",
        "export" | "export_esm" | "export_cjs" => "exports",
        "type_reference" => "references",
        "jsx_component" => "renders",
        "blade_view" | "blade_component" | "blade_x_component" => "renders",
        "decorator" => "references",
        _ => "references",
    }
}

/// Rewrite import-style refs in an `ExtractedFile` so their `qname` reflects
/// the project's TypeScript path aliases / Python package layout, and so
/// per-binding refs carry a composite `<resolved_path>#<symbol>` (JS/TS) or
/// `<resolved_module>.<symbol>` (Python) key that matches the registrations
/// added by `build_symbol_table`. Refs that don't resolve (external
/// packages, stdlib) are left with their raw source so the symbol table
/// lookup simply misses, producing no edge.
fn rewrite_imports(
    extracted: &mut ExtractedFile,
    relative_path: &Path,
    language: DetectedLanguage,
    ts: &TsAliasResolver,
    py: &PythonImportResolver,
    worktree_root: &Path,
) {
    match language {
        DetectedLanguage::JavaScript | DetectedLanguage::TypeScript | DetectedLanguage::Tsx => {
            let local_prefix = js_path_key(relative_path);
            for r in &mut extracted.refs {
                match r.kind.as_str() {
                    // Module-level refs: name carries the raw import string,
                    // qname starts unset.
                    "import_esm" | "import_cjs" => {
                        if let Some(resolved) = ts.resolve(&r.name, relative_path, worktree_root) {
                            r.qname = Some(resolved);
                        }
                    }
                    // Per-binding refs: qname carries the raw module source
                    // from the extractor; rewrite to `<resolved>#<symbol>`.
                    "import_named" | "import_default" | "import_namespace" => {
                        if let Some(module_src) = r.qname.clone()
                            && let Some(resolved) =
                                ts.resolve(&module_src, relative_path, worktree_root)
                        {
                            r.qname = Some(format!("{resolved}#{}", r.name));
                        }
                    }
                    // Imported call/render refs leave the extractor as
                    // `<raw_module>#<symbol>`. Local identifier call/render
                    // refs get a file-local key. Either path avoids the
                    // old repo-wide bare-name fallback.
                    "call" | "jsx_component" => {
                        if rewrite_js_import_ref_qname(r, relative_path, ts, worktree_root) {
                            continue;
                        }
                        if r.qname.is_none() && is_js_simple_identifier(&r.name) {
                            r.qname = Some(js_local_symbol_key(&local_prefix, &r.name));
                        }
                    }
                    // Member refs only resolve when they came from a
                    // namespace import (`Utils.format()` / `<Icons.Home />`).
                    // Ordinary property names such as `.map()` stay
                    // unresolved, so they cannot fan out to unrelated symbols.
                    "member_call" | "member_access" => {
                        let _ = rewrite_js_import_ref_qname(r, relative_path, ts, worktree_root);
                    }
                    _ => {}
                }
            }
        }
        DetectedLanguage::Python => {
            for r in &mut extracted.refs {
                if r.kind != "import" {
                    continue;
                }
                // Two emission shapes from python.rs:
                //   `import os`             → name="os",  qname=None
                //   `from .helper import X` → name="X",   qname=Some(".helper.X")
                //   `from ..pkg import *`   → name="*",   qname=Some("..pkg.*")
                // In all cases the resolver wants the module portion only.
                if let Some(existing_qname) = r.qname.clone() {
                    let suffix = format!(".{}", r.name);
                    let module = existing_qname
                        .strip_suffix(&suffix)
                        .unwrap_or(&existing_qname)
                        .to_owned();
                    if let Some(resolved_module) = py.resolve(relative_path, &module) {
                        r.qname = Some(format!("{resolved_module}.{}", r.name));
                    }
                } else if let Some(resolved) = py.resolve(relative_path, &r.name) {
                    r.qname = Some(resolved);
                }
            }
        }
        DetectedLanguage::Php | DetectedLanguage::Blade => {}
    }
}

fn rewrite_js_import_ref_qname(
    r: &mut crate::extract::Ref,
    relative_path: &Path,
    ts: &TsAliasResolver,
    worktree_root: &Path,
) -> bool {
    let Some(qname) = r.qname.clone() else {
        return false;
    };
    let Some((module_src, symbol)) = qname.split_once('#') else {
        return false;
    };
    let Some(resolved) = ts.resolve(module_src, relative_path, worktree_root) else {
        return false;
    };
    r.qname = Some(format!("{resolved}#{symbol}"));
    true
}

fn is_js_simple_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn framework_edge_kind(kind: crate::laravel::FrameworkEdgeKind) -> &'static str {
    use crate::laravel::FrameworkEdgeKind::*;
    match kind {
        RouteToController => "routes_to",
        ControllerToModel => "uses_model",
        EloquentRelationship => "relates_to",
        FacadeCall => "facade_call",
        ServiceBinding => "binds",
        EventListener => "dispatches_event",
        JobDispatch => "dispatches_job",
        BladeExtendsView => "extends_view",
        BladeIncludesView => "includes_view",
        BladeUsesComponent => "uses_component",
        ReactComponent => "react_component",
        ReactHook => "react_hook",
        ReactUsesHook => "react_uses_hook",
    }
}

/// Append a `LaravelFacts.edges` batch to the edge fact list. Framework
/// edges synthesize stable string node IDs prefixed with `lh:` so they
/// cannot collide with parser-extracted IDs (always 64-hex-char content
/// hash prefixes).
fn append_framework_edges(facts: &crate::laravel::LaravelFacts, edges: &mut Vec<EdgeFact>) {
    for fedge in &facts.edges {
        let source_id = format!("lh:{}", fedge.from_qname);
        let target_id = format!("lh:{}", fedge.to_qname);
        edges.push(EdgeFact {
            source_node_id: source_id,
            kind: framework_edge_kind(fedge.kind).to_string(),
            target_node_id: target_id,
            provenance: "laravel_heuristic".to_string(),
            confidence: framework_confidence(fedge.confidence),
        });
    }
}

/// Translate the canonical Blade refs (`blade_extends`, `blade_view`,
/// `blade_component`, `blade_x_component`) into the laravel resolver's
/// `BladeRef` shape.
fn blade_input_from_extracted(
    relative: &Path,
    extracted: &ExtractedFile,
) -> crate::laravel::BladeExtractInput {
    use crate::laravel::{BladeRef, BladeRefKind, Span as LavSpan};
    let mut refs = Vec::new();
    for r in &extracted.refs {
        let kind = match r.kind.as_str() {
            "blade_extends" => BladeRefKind::ExtendsView,
            "blade_view" => BladeRefKind::IncludesView,
            "blade_component" => BladeRefKind::Component,
            "blade_x_component" => BladeRefKind::XComponent,
            _ => continue,
        };
        refs.push(BladeRef {
            kind,
            value: r.name.clone(),
            span: LavSpan {
                start: r.span.start.byte,
                end: r.span.end.byte,
            },
        });
    }
    crate::laravel::BladeExtractInput {
        path: relative.to_path_buf(),
        refs,
    }
}

fn framework_confidence(conf: crate::laravel::Confidence) -> u32 {
    use crate::laravel::Confidence::*;
    match conf {
        Low => 40,
        Medium => 70,
        High => 90,
    }
}

fn mtime_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_secs() as i64,
        Err(err) => -(err.duration().as_secs() as i64),
    }
}

fn language_label(id: DetectedLanguage) -> &'static str {
    match id {
        DetectedLanguage::Php => "php",
        DetectedLanguage::Blade => "blade",
        DetectedLanguage::JavaScript => "javascript",
        DetectedLanguage::TypeScript => "typescript",
        DetectedLanguage::Tsx => "tsx",
        DetectedLanguage::Python => "python",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn index_all_submits_one_update_per_supported_file() {
        let tmp = TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("a.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "no language").unwrap();

        let matcher = IgnoreMatcher::new(tmp.path()).expect("matcher");
        let registry = LanguageRegistry::with_all();
        let store_dir = tmp.path().join(".xgraph-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CozoStore::open(&store_dir).expect("cozo");

        let indexes = Arc::new(HotIndexes::new());
        let status = Arc::new(DaemonStatus::new());
        let mut owner = WorktreeOwner::new(
            tmp.path().to_path_buf(),
            matcher,
            registry,
            store,
            indexes,
            status,
        )
        .expect("owner");
        let summary = owner.index_all().expect("index_all");
        assert_eq!(
            summary.files_indexed, 1,
            "only the .py file should produce an update"
        );
        let errs = owner.shutdown();
        assert!(errs.is_empty(), "writer errors: {errs:?}");
    }

    #[test]
    fn index_all_is_idempotent_thanks_to_hash_skip() {
        let tmp = TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("a.py"), "def f():\n    return 1\n").unwrap();

        let matcher = IgnoreMatcher::new(tmp.path()).expect("matcher");
        let registry = LanguageRegistry::with_all();
        let store_dir = tmp.path().join(".xgraph-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CozoStore::open(&store_dir).expect("cozo");
        let indexes = Arc::new(HotIndexes::new());

        let matcher2 = IgnoreMatcher::new(tmp.path()).expect("matcher2");
        let registry2 = LanguageRegistry::with_all();

        let mut owner = WorktreeOwner::new(
            tmp.path().to_path_buf(),
            matcher,
            registry,
            store.clone(),
            Arc::clone(&indexes),
            Arc::new(DaemonStatus::new()),
        )
        .expect("owner");
        let first = owner.index_all().expect("index_all first");
        assert_eq!(first.files_indexed, 1);
        let errs = owner.shutdown();
        assert!(errs.is_empty());

        // Second pass: file unchanged → 0 submissions.
        let mut owner2 = WorktreeOwner::new(
            tmp.path().to_path_buf(),
            matcher2,
            registry2,
            store,
            Arc::clone(&indexes),
            Arc::new(DaemonStatus::new()),
        )
        .expect("owner2");
        let second = owner2.index_all().expect("index_all second");
        assert_eq!(
            second.files_indexed, 0,
            "hash-skip should suppress re-extraction"
        );
        assert!(owner2.shutdown().is_empty());
    }

    #[test]
    fn process_change_skips_paths_without_language() {
        let tmp = TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("README.md"), "# hi\n").unwrap();

        let matcher = IgnoreMatcher::new(tmp.path()).expect("matcher");
        let registry = LanguageRegistry::with_all();
        let store_dir = tmp.path().join(".xgraph-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CozoStore::open(&store_dir).expect("cozo");

        let indexes = Arc::new(HotIndexes::new());
        let status = Arc::new(DaemonStatus::new());
        let mut owner = WorktreeOwner::new(
            tmp.path().to_path_buf(),
            matcher,
            registry,
            store,
            indexes,
            status,
        )
        .expect("owner");
        let changed = owner
            .process_change(tmp.path().join("README.md"))
            .expect("process_change");
        assert!(!changed);
        let errs = owner.shutdown();
        assert!(errs.is_empty());
    }
}
