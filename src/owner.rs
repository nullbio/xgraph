//! Per-worktree owner that bundles the daemon's long-lived dependencies.
//!
//! Owns the Cozo store, writer queue, ignore matcher, language registry, and
//! the parser-version constant used in content fact rows. Provides the
//! daemon-side primitives for initial indexing and incremental updates.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cozo::{
    ContentHash as CozoContentHash, CozoStore, EdgeFact, FileUpdate, FileUpdateMetadata,
    WriterError, WriterHandle, WriterQueue, active_node_id,
};
use crate::extract::ExtractedFile;
use crate::hash::ContentHash;
use crate::ignore::IgnoreMatcher;
use crate::language::LanguageRegistry;
use crate::scanner::{DetectedLanguage, ScanError, scan};

pub const PARSER_VERSION: u32 = 1;

pub struct WorktreeOwner {
    worktree_root: PathBuf,
    matcher: IgnoreMatcher,
    registry: LanguageRegistry,
    writer: WriterHandle,
    generation: u64,
}

impl WorktreeOwner {
    /// Build an owner against a freshly-opened Cozo store. Caller supplies
    /// the store; the owner takes responsibility for the writer queue.
    pub fn new(
        worktree_root: PathBuf,
        matcher: IgnoreMatcher,
        registry: LanguageRegistry,
        store: CozoStore,
    ) -> Result<Self, crate::cozo::CozoError> {
        let writer = WriterQueue::start(store)?;
        Ok(Self {
            worktree_root,
            matcher,
            registry,
            writer,
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
        Ok(count)
    }

    /// Re-extract a single path and submit the update. Cross-file resolution
    /// uses an empty symbol table (callers will be resolved on the next
    /// scheduled reconciliation pass).
    pub fn process_change(&mut self, path: PathBuf) -> Result<bool, OwnerError> {
        if !path.exists() {
            return Ok(false);
        }
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
        let Some(extracted) = self.registry.extract_file(&relative, &bytes) else {
            return Ok(None);
        };
        Ok(Some(PreparedFile {
            relative,
            content_hash,
            language,
            mtime,
            size,
            extracted,
        }))
    }

    fn submit_prepared(
        &mut self,
        prep: PreparedFile,
        symbols: &SymbolTable,
    ) -> Result<(), OwnerError> {
        let cozo_hash = CozoContentHash::from_bytes(*prep.content_hash.as_bytes());
        let edges = resolve_edges(&prep.content_hash, &prep.extracted, symbols);

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
        self.writer.submit(update)?;
        Ok(())
    }

    /// Drain pending submissions and return any errors the writer thread recorded.
    pub fn shutdown(mut self) -> Vec<WriterError> {
        self.writer.shutdown();
        self.writer.take_errors()
    }
}

#[derive(Debug)]
pub enum OwnerError {
    Scan(ScanError),
    Cozo(crate::cozo::CozoError),
    Writer(WriterError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for OwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnerError::Scan(err) => write!(f, "{err}"),
            OwnerError::Cozo(err) => write!(f, "{err}"),
            OwnerError::Writer(err) => write!(f, "{err}"),
            OwnerError::Io { path, source } => {
                write!(f, "io error on {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for OwnerError {}

impl From<ScanError> for OwnerError {
    fn from(err: ScanError) -> Self {
        OwnerError::Scan(err)
    }
}

impl From<crate::cozo::CozoError> for OwnerError {
    fn from(err: crate::cozo::CozoError) -> Self {
        OwnerError::Cozo(err)
    }
}

impl From<WriterError> for OwnerError {
    fn from(err: WriterError) -> Self {
        OwnerError::Writer(err)
    }
}

struct PreparedFile {
    relative: PathBuf,
    content_hash: ContentHash,
    language: DetectedLanguage,
    mtime: SystemTime,
    size: u64,
    extracted: ExtractedFile,
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

        let mut owner =
            WorktreeOwner::new(tmp.path().to_path_buf(), matcher, registry, store).expect("owner");
        let n = owner.index_all().expect("index_all");
        assert_eq!(n, 1, "only the .py file should produce an update");
        let errs = owner.shutdown();
        assert!(errs.is_empty(), "writer errors: {errs:?}");
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

        let mut owner =
            WorktreeOwner::new(tmp.path().to_path_buf(), matcher, registry, store).expect("owner");
        let changed = owner
            .process_change(tmp.path().join("README.md"))
            .expect("process_change");
        assert!(!changed);
        let errs = owner.shutdown();
        assert!(errs.is_empty());
    }
}
