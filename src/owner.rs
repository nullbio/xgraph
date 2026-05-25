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
use crate::indexes::HotIndexes;
use crate::language::LanguageRegistry;
use crate::scanner::{DetectedLanguage, ScanError, scan};

pub const PARSER_VERSION: u32 = 1;

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
    /// Returns the number of files submitted to the writer queue.
    pub fn index_all(&mut self) -> Result<usize, OwnerError> {
        let scanned = scan(&self.worktree_root, &self.matcher)?;

        let mut prepared: Vec<PreparedFile> = Vec::with_capacity(scanned.len());
        for file in scanned {
            let Some(lang) = file.language else { continue };
            if let Some(p) = self.prepare_file(file.path, file.mtime, file.size, lang)? {
                prepared.push(p);
            }
        }

        let symbol_table = build_symbol_table(&prepared);
        let count = prepared.len();
        for prep in prepared {
            self.submit_prepared(prep, &symbol_table)?;
        }
        // After the initial walk completes, MCP queries no longer need the
        // "still booting" caveat. Incremental change-driven catching_up is
        // tracked per-path via DaemonStatus::pending_paths.
        self.status.mark_reconcile_done();
        Ok(count)
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
        self.submit_prepared(prep, &empty)?;
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
    ) -> Result<(), OwnerError> {
        let cozo_hash = CozoContentHash::from_bytes(*prep.content_hash.as_bytes());
        let mut edges = resolve_edges(&prep.content_hash, &prep.extracted, symbols);

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
            for fedge in facts.edges {
                // Framework edges synthesize stable string IDs so they can
                // reference symbols that may live in other files. The
                // "lh:" prefix marks them as laravel-heuristic facts.
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
        self.writer.submit(update)?;
        Ok(())
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

/// Maps a qualified name to the set of global node ids that own it.
#[derive(Default)]
struct SymbolTable {
    by_qname: HashMap<String, Vec<String>>,
}

impl SymbolTable {
    fn register(&mut self, qname: &str, node_id: String) {
        if qname.is_empty() {
            return;
        }
        self.by_qname
            .entry(qname.to_owned())
            .or_default()
            .push(node_id);
    }

    fn lookup(&self, qname: &str) -> &[String] {
        self.by_qname.get(qname).map(Vec::as_slice).unwrap_or(&[])
    }
}

fn build_symbol_table(prepared: &[PreparedFile]) -> SymbolTable {
    let mut table = SymbolTable::default();
    for prep in prepared {
        let cozo_hash = CozoContentHash::from_bytes(*prep.content_hash.as_bytes());
        for node in &prep.extracted.nodes {
            let node_id = active_node_id(&cozo_hash, node.id);
            table.register(&node.qname, node_id.clone());
            // Also index by bare name when it differs, so unqualified callers can resolve.
            if node.name != node.qname {
                table.register(&node.name, node_id);
            }
        }
    }
    table
}

fn resolve_edges(
    hash: &ContentHash,
    extracted: &ExtractedFile,
    symbols: &SymbolTable,
) -> Vec<EdgeFact> {
    let cozo_hash = CozoContentHash::from_bytes(*hash.as_bytes());
    let mut edges = Vec::new();
    for r in &extracted.refs {
        let lookup_key = r.qname.as_deref().unwrap_or(r.name.as_str());
        let targets = symbols.lookup(lookup_key);
        if targets.is_empty() {
            continue;
        }
        let edge_kind = edge_kind_for_ref(&r.kind);
        // Use the ref's container as the edge source if present; otherwise
        // attribute the edge to the file's first top-level node, falling back
        // to a synthetic source rooted at the file's content hash.
        let source_node_id = match r.container {
            Some(local) => active_node_id(&cozo_hash, local),
            None => continue,
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

fn edge_kind_for_ref(kind: &str) -> &str {
    match kind {
        "call" | "method_call" | "static_call" | "nullsafe_method_call" => "calls",
        "extends" | "inheritance" => "inherits",
        "implements" => "implements",
        "trait_use" => "uses",
        "import" | "import_esm" | "import_cjs" => "imports",
        "export" | "export_esm" | "export_cjs" => "exports",
        "type_reference" => "references",
        "jsx_component" => "renders",
        "blade_view" | "blade_component" | "blade_x_component" => "renders",
        "decorator" => "references",
        _ => "references",
    }
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
        let n = owner.index_all().expect("index_all");
        assert_eq!(n, 1, "only the .py file should produce an update");
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
        assert_eq!(first, 1);
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
        assert_eq!(second, 0, "hash-skip should suppress re-extraction");
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
