//! Embedded CozoDB schema and the single-writer queue.
//!
//! This module owns the on-disk CozoDB engine and the only thread permitted to
//! mutate it. All graph mutations flow through [`WriterHandle::submit`], are
//! serialized by a dedicated worker thread, and are applied as one CozoScript
//! transaction per file update so readers never observe a half-updated state.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use cozo::{DataValue, DbInstance, JsonData, NamedRows, Num, ScriptMutability};
use crossbeam_channel::{Sender, TrySendError, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Current schema version. Bumped whenever the relation set changes.
pub const SCHEMA_VERSION: u32 = 1;

/// Bounded sentinel used as the sole key of the `schema_version` singleton.
const SCHEMA_VERSION_KEY: i64 = 0;

/// Length of a content hash in bytes (SHA-256).
pub const CONTENT_HASH_LEN: usize = 32;

/// Content-addressed hash for a file.
///
/// A minimal local wrapper around 32 bytes. The full hashing implementation
/// lives in another module; this type exists only so the writer queue does not
/// have to cross-import.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; CONTENT_HASH_LEN]);

impl ContentHash {
    pub fn from_bytes(bytes: [u8; CONTENT_HASH_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; CONTENT_HASH_LEN] {
        &self.0
    }

    pub fn to_vec(self) -> Vec<u8> {
        self.0.to_vec()
    }
}

/// A byte-range span within a parsed file.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_row: u32,
    pub start_col: u32,
}

impl Span {
    fn to_data_value(self) -> DataValue {
        DataValue::List(vec![
            DataValue::from(i64::from(self.start_byte)),
            DataValue::from(i64::from(self.end_byte)),
            DataValue::from(i64::from(self.start_row)),
            DataValue::from(i64::from(self.start_col)),
        ])
    }
}

/// A parsed node from a single file's content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeFact {
    pub local_node_id: u32,
    pub kind: String,
    pub name: String,
    pub qname: String,
    pub span: Span,
}

/// A parsed reference from a single file's content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefFact {
    pub local_ref_id: u32,
    pub kind: String,
    pub name: String,
    pub span: Span,
}

/// A graph edge between two active nodes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EdgeFact {
    pub source_node_id: String,
    pub kind: String,
    pub target_node_id: String,
    pub provenance: String,
    pub confidence: u32,
}

/// A diagnostic emitted by the parser or an extractor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: String,
    pub message: String,
    pub span: Option<Span>,
}

/// One transactional replacement of a single file's facts.
#[derive(Clone, Debug)]
pub struct FileUpdate {
    pub path: String,
    pub content_hash: ContentHash,
    pub language: String,
    pub parser_version: u32,
    pub mtime: i64,
    pub size: u64,
    pub generation: u64,
    pub diagnostics: Vec<Diagnostic>,
    pub nodes: Vec<NodeFact>,
    pub refs: Vec<RefFact>,
    pub edges: Vec<EdgeFact>,
}

/// Metadata that must be supplied alongside an `ExtractedFile` to build a `FileUpdate`.
pub struct FileUpdateMetadata {
    pub content_hash: ContentHash,
    pub language: String,
    pub parser_version: u32,
    pub mtime: i64,
    pub size: u64,
    pub generation: u64,
}

impl FileUpdate {
    /// Convert a canonical `ExtractedFile` plus storage metadata into a writer-queue `FileUpdate`.
    /// Cross-file resolution (edges) is performed elsewhere; this conversion produces an empty
    /// `edges` list. Language-extractor diagnostics are translated; spans drop their `end_row`/
    /// `end_column` because Cozo's row schema captures only the start position.
    pub fn from_extracted(
        extracted: &crate::extract::ExtractedFile,
        metadata: FileUpdateMetadata,
    ) -> Self {
        let nodes = extracted
            .nodes
            .iter()
            .map(|n| NodeFact {
                local_node_id: n.id,
                kind: n.kind.clone(),
                name: n.name.clone(),
                qname: n.qname.clone(),
                span: span_from_extract(n.span),
            })
            .collect();
        let refs = extracted
            .refs
            .iter()
            .map(|r| RefFact {
                local_ref_id: r.id,
                kind: r.kind.clone(),
                name: r.name.clone(),
                span: span_from_extract(r.span),
            })
            .collect();
        let diagnostics = extracted
            .diagnostics
            .iter()
            .map(|d| Diagnostic {
                severity: severity_label(d.severity).to_owned(),
                message: d.message.clone(),
                span: d.span.map(span_from_extract),
            })
            .collect();
        Self {
            path: extracted.path.to_string_lossy().into_owned(),
            content_hash: metadata.content_hash,
            language: metadata.language,
            parser_version: metadata.parser_version,
            mtime: metadata.mtime,
            size: metadata.size,
            generation: metadata.generation,
            diagnostics,
            nodes,
            refs,
            edges: Vec::new(),
        }
    }
}

fn span_from_extract(span: crate::extract::Span) -> Span {
    Span {
        start_byte: span.start.byte as u32,
        end_byte: span.end.byte as u32,
        start_row: span.start.row as u32,
        start_col: span.start.column as u32,
    }
}

fn severity_label(severity: crate::extract::Severity) -> &'static str {
    match severity {
        crate::extract::Severity::Error => "error",
        crate::extract::Severity::Warning => "warning",
    }
}

/// Errors returned when opening the store, running queries, or driving the
/// writer queue.
#[derive(Debug)]
pub enum CozoError {
    /// Wrapper around any error reported by the embedded engine.
    Engine(String),
    /// JSON (de)serialization for payloads stored in the database failed.
    Json(serde_json::Error),
    /// The query returned a value we cannot decode into the expected shape.
    UnexpectedShape(String),
}

impl std::fmt::Display for CozoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(message) => write!(f, "cozo engine error: {message}"),
            Self::Json(err) => write!(f, "cozo json error: {err}"),
            Self::UnexpectedShape(message) => write!(f, "cozo unexpected shape: {message}"),
        }
    }
}

impl std::error::Error for CozoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for CozoError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn engine_error<T: std::fmt::Display>(err: T) -> CozoError {
    CozoError::Engine(err.to_string())
}

/// Errors returned by the writer queue handle.
#[derive(Debug)]
pub enum WriterError {
    /// The worker thread has already terminated and is no longer accepting
    /// submissions.
    QueueClosed,
    /// The worker reported an unrecoverable error while applying the
    /// transaction; the channel is preserved so further submissions can still
    /// surface follow-up failures.
    Apply(CozoError),
}

impl std::fmt::Display for WriterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueClosed => write!(f, "writer queue is closed"),
            Self::Apply(err) => write!(f, "writer apply error: {err}"),
        }
    }
}

impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Apply(err) => Some(err),
            Self::QueueClosed => None,
        }
    }
}

/// Owns the embedded CozoDB engine for one worktree.
///
/// The store does not enforce single-writer access by itself; callers must use
/// [`WriterQueue`] for every mutation so the daemon-wide single-writer
/// invariant holds.
#[derive(Clone)]
pub struct CozoStore {
    db: Arc<DbInstance>,
}

impl CozoStore {
    /// Open the on-disk RocksDB-backed engine at `path`, install any missing
    /// relations, and ensure the schema version row is up to date.
    pub fn open(path: &Path) -> Result<Self, CozoError> {
        let path_str = path.to_str().ok_or_else(|| {
            CozoError::Engine(format!("cozo path is not valid UTF-8: {}", path.display()))
        })?;
        let db = DbInstance::new("rocksdb", path_str, "").map_err(engine_error)?;
        let store = Self { db: Arc::new(db) };
        store.install_schema()?;
        Ok(store)
    }

    /// Return the schema version recorded in the database.
    ///
    /// Returns zero if the singleton row is missing (e.g. the relation exists
    /// but has not been populated yet). Engine-level read failures are
    /// surfaced rather than silently masked.
    pub fn schema_version(&self) -> Result<u32, CozoError> {
        let rows = self.run_immutable(
            "?[version] := *schema_version[_singleton, version]",
            BTreeMap::new(),
        )?;
        let Some(first) = rows.rows.into_iter().next() else {
            return Ok(0);
        };
        let value = first.into_iter().next().ok_or_else(|| {
            CozoError::UnexpectedShape("schema_version row was empty".to_string())
        })?;
        data_value_as_u32(value).ok_or_else(|| {
            CozoError::UnexpectedShape("schema_version was not a u32-sized integer".to_string())
        })
    }

    /// Run a script that may mutate the database.
    fn run_mutable(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows, CozoError> {
        self.db
            .run_script(script, params, ScriptMutability::Mutable)
            .map_err(engine_error)
    }

    /// Run a read-only script.
    fn run_immutable(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows, CozoError> {
        self.db
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(engine_error)
    }

    /// Run a read-only CozoScript query against the store.
    ///
    /// Other modules (hot indexes, MCP request handlers, etc.) use this
    /// instead of touching the underlying [`DbInstance`] so the single-writer
    /// invariant cannot be bypassed accidentally.
    pub fn run_read(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows, CozoError> {
        self.run_immutable(script, params)
    }

    /// Drop every fact relation, leaving the schema intact. Used by
    /// `xgraph reindex` to rebuild the graph from scratch.
    pub fn truncate_graph(&self) -> Result<(), CozoError> {
        // Drop secondary indexes before their parent relations.
        // `::remove <rel>` errors on a relation that still has an index
        // attached, so the prior `::index drop` is load-bearing for
        // `cmd_reindex` correctness.
        for (rel, idx) in [("active_node", "by_path"), ("symbol", "by_path")] {
            let _ = self.run_mutable(&format!("::index drop {rel}:{idx}"), BTreeMap::new());
        }
        // `:rm` rules without a key clause delete all rows.
        for rel in [
            "content_file",
            "content_node",
            "content_ref",
            "active_file",
            "active_node",
            "edge",
            "symbol",
        ] {
            let script = format!("?[a, b, c] := *{rel}{{a, b, c}} \n:rm {rel} {{a, b, c}}");
            // Some relations have different arity; try the broad delete and
            // ignore relation-shape errors. The simpler `::remove <rel>` op
            // drops the whole relation, but we want to keep the schema.
            let _ = self.run_mutable(&script, BTreeMap::new());
        }
        // The above tolerant approach is unreliable for relations with arity
        // ≠ 3, so also issue per-relation explicit removes via the system op.
        for rel in [
            "content_file",
            "content_node",
            "content_ref",
            "active_file",
            "active_node",
            "edge",
            "symbol",
        ] {
            let _ = self.run_mutable(&format!("::remove {rel}"), BTreeMap::new());
        }
        // Reinstall the schema so the now-removed relations come back empty.
        self.install_schema()?;
        Ok(())
    }

    /// Look up the active content hash stored for `path`. Returns `None` if
    /// the file has never been indexed.
    ///
    /// Used by the hash-skip cache: if a freshly-computed hash equals the
    /// stored hash, parsing + extraction can be skipped entirely.
    pub fn active_file_hash(&self, path: &str) -> Result<Option<ContentHash>, CozoError> {
        let mut params = BTreeMap::new();
        params.insert("path".to_string(), DataValue::from(path.to_string()));
        let rows = self.run_immutable(
            "?[hash] := *active_file[$path, hash, _mtime, _size, _generation]",
            params,
        )?;
        let Some(first) = rows.rows.into_iter().next() else {
            return Ok(None);
        };
        let Some(DataValue::Bytes(bytes)) = first.into_iter().next() else {
            return Ok(None);
        };
        if bytes.len() != CONTENT_HASH_LEN {
            return Ok(None);
        }
        let mut out = [0u8; CONTENT_HASH_LEN];
        out.copy_from_slice(&bytes);
        Ok(Some(ContentHash::from_bytes(out)))
    }

    fn install_schema(&self) -> Result<(), CozoError> {
        let existing = self.list_relations()?;
        for (name, script) in RELATION_DDL {
            if !existing.contains(*name) {
                self.run_mutable(script, BTreeMap::new())?;
            }
        }
        // Secondary indexes on the columns the per-file cleanup phase
        // uses for lookups. Without these, every file update did a full
        // scan of active_node / symbol filtering by `path`, which made
        // store-phase O(N_existing_rows × N_paths_in_batch). With
        // them it's O(matching_rows). Cozo errors if the index already
        // exists, so we tolerate that and treat anything else as fatal.
        for (rel, idx, cols) in [
            ("active_node", "by_path", "path"),
            ("symbol", "by_path", "path"),
        ] {
            let script = format!("::index create {rel}:{idx} {{{cols}}}");
            match self.run_mutable(&script, BTreeMap::new()) {
                Ok(_) => {}
                Err(err) => {
                    let msg = err.to_string();
                    // Cozo reports a specific phrase for "already exists";
                    // anything else is a real failure we should surface.
                    if !msg.contains("already exists") && !msg.contains("exists already") {
                        return Err(err);
                    }
                }
            }
        }
        // After the relation set is guaranteed to exist, read the stored
        // version and only write the row when it differs. Future migrations
        // will run between these two points.
        let stored = self.schema_version()?;
        if stored != SCHEMA_VERSION {
            self.write_schema_version(SCHEMA_VERSION)?;
        }
        Ok(())
    }

    fn list_relations(&self) -> Result<std::collections::BTreeSet<String>, CozoError> {
        let rows = self.run_immutable("::relations", BTreeMap::new())?;
        let mut names = std::collections::BTreeSet::new();
        for row in rows.rows {
            let first = row.into_iter().next().ok_or_else(|| {
                CozoError::UnexpectedShape("::relations row was empty".to_string())
            })?;
            let name = data_value_as_string(first).ok_or_else(|| {
                CozoError::UnexpectedShape("::relations name was not a string".to_string())
            })?;
            names.insert(name);
        }
        Ok(names)
    }

    fn write_schema_version(&self, version: u32) -> Result<(), CozoError> {
        let mut params = BTreeMap::new();
        params.insert("singleton".to_string(), DataValue::from(SCHEMA_VERSION_KEY));
        params.insert("version".to_string(), DataValue::from(i64::from(version)));
        self.run_mutable(
            "?[singleton, version] <- [[$singleton, $version]] :put schema_version {singleton => version}",
            params,
        )?;
        Ok(())
    }
}

fn data_value_as_string(value: DataValue) -> Option<String> {
    match value {
        DataValue::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

fn data_value_as_u32(value: DataValue) -> Option<u32> {
    match value {
        DataValue::Num(Num::Int(i)) if (0..=i64::from(u32::MAX)).contains(&i) => Some(i as u32),
        _ => None,
    }
}

/// Stored relation DDL keyed by relation name.
///
/// The order matters only for first-install readability; missing relations are
/// created independently on every open.
const RELATION_DDL: &[(&str, &str)] = &[
    (
        "content_file",
        ":create content_file {\n    content_hash: Bytes,\n    =>\n    language: String,\n    parser_version: Int,\n    diagnostics: Json,\n}",
    ),
    (
        "content_node",
        ":create content_node {\n    content_hash: Bytes,\n    local_node_id: Int,\n    =>\n    kind: String,\n    name: String,\n    qname: String,\n    span: [Int],\n}",
    ),
    (
        "content_ref",
        ":create content_ref {\n    content_hash: Bytes,\n    local_ref_id: Int,\n    =>\n    kind: String,\n    name: String,\n    span: [Int],\n}",
    ),
    (
        "active_file",
        ":create active_file {\n    path: String,\n    =>\n    content_hash: Bytes,\n    mtime: Int,\n    size: Int,\n    generation: Int,\n}",
    ),
    (
        "active_node",
        ":create active_node {\n    node_id: String,\n    =>\n    path: String,\n    content_hash: Bytes,\n    local_node_id: Int,\n    kind: String,\n    name: String,\n    qname: String,\n    span: [Int],\n}",
    ),
    (
        "edge",
        ":create edge {\n    source_node_id: String,\n    kind: String,\n    target_node_id: String,\n    =>\n    provenance: String,\n    confidence: Int,\n}",
    ),
    (
        "symbol",
        ":create symbol {\n    name: String,\n    kind: String,\n    node_id: String,\n    =>\n    qname: String,\n    path: String,\n}",
    ),
    (
        "schema_version",
        ":create schema_version {\n    singleton: Int,\n    =>\n    version: Int,\n}",
    ),
];

/// Builds the deterministic active-node identifier used as the primary key of
/// `active_node` and the source/target of edges.
pub fn active_node_id(hash: &ContentHash, local_node_id: u32) -> String {
    let mut out = String::with_capacity(CONTENT_HASH_LEN * 2 + 1 + 10);
    for byte in hash.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out.push(':');
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{local_node_id}"));
    out
}

/// Internal message sent to the writer thread.
///
/// `Update` boxes its payload because `FileUpdate` is significantly larger
/// than the unit `Shutdown` variant; keeping the enum small reduces the
/// per-message channel overhead.
enum WriterMessage {
    Update(Box<FileUpdate>),
    Truncate(crossbeam_channel::Sender<Result<(), WriterError>>),
    /// Sync barrier: the writer commits everything submitted before this
    /// message, then signals the bundled `Sender`. Used by callers that
    /// need a "writer has drained" point — e.g., `init_at` reporting
    /// real wall time including the database commit.
    Flush(crossbeam_channel::Sender<()>),
    Shutdown,
}

/// Shared bag of writer errors. Callers drain it via
/// [`WriterHandle::take_errors`] to observe transactional failures the worker
/// could not surface synchronously through [`WriterHandle::submit`].
type ErrorSink = Arc<Mutex<Vec<WriterError>>>;

/// A handle to the writer worker thread.
///
/// Dropping the handle without calling [`Self::shutdown`] still drains
/// in-flight submissions because the channel is closed, which triggers the
/// worker's exit path. Calling [`Self::shutdown`] explicitly is preferred so
/// the worker can flush errors and the join error path runs deterministically.
pub struct WriterHandle {
    sender: Option<Sender<WriterMessage>>,
    worker: Option<JoinHandle<()>>,
    errors: ErrorSink,
}

impl WriterHandle {
    /// Enqueue a file update for asynchronous application. Returns immediately
    /// without waiting for the transaction to commit.
    pub fn submit(&self, update: FileUpdate) -> Result<(), WriterError> {
        let sender = self.sender.as_ref().ok_or(WriterError::QueueClosed)?;
        match sender.try_send(WriterMessage::Update(Box::new(update))) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(WriterError::QueueClosed),
            // An unbounded channel never returns Full, but be defensive in case
            // the channel type is changed later.
            Err(TrySendError::Full(_)) => Err(WriterError::QueueClosed),
        }
    }

    /// Drain and return every error the worker recorded since the previous
    /// call. Useful for surfacing async transaction failures into metrics or
    /// logs without forcing every caller to await each write.
    pub fn take_errors(&self) -> Vec<WriterError> {
        match self.errors.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            // Lock poisoning means a worker thread panicked while holding the
            // lock. Recover the inner Vec so we still drain the errors.
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }

    /// Block until every previously-submitted update has been committed.
    /// Idempotent — calling on an already-drained queue returns
    /// immediately. Returns `WriterError::QueueClosed` if the worker has
    /// already shut down.
    pub fn flush(&self) -> Result<(), WriterError> {
        let sender = self.sender.as_ref().ok_or(WriterError::QueueClosed)?;
        let (tx, rx) = crossbeam_channel::bounded(1);
        sender
            .send(WriterMessage::Flush(tx))
            .map_err(|_| WriterError::QueueClosed)?;
        rx.recv().map_err(|_| WriterError::QueueClosed)?;
        Ok(())
    }

    pub fn truncate_graph(&self) -> Result<(), WriterError> {
        let sender = self.sender.as_ref().ok_or(WriterError::QueueClosed)?;
        let (tx, rx) = crossbeam_channel::bounded(1);
        sender
            .send(WriterMessage::Truncate(tx))
            .map_err(|_| WriterError::QueueClosed)?;
        rx.recv().map_err(|_| WriterError::QueueClosed)?
    }

    /// Drain pending submissions, signal the worker to stop, and join the
    /// thread. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            // Best-effort signal; if the worker has already exited we still
            // join below to surface a panic.
            let _ = sender.send(WriterMessage::Shutdown);
            drop(sender);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawns the single writer worker thread.
pub struct WriterQueue;

impl WriterQueue {
    /// Start the writer worker for `store`. The worker owns the only mutable
    /// access to the store. Returns an error only if the operating system
    /// refuses to spawn the writer thread.
    pub fn start(store: CozoStore) -> Result<WriterHandle, CozoError> {
        let (sender, receiver) = unbounded::<WriterMessage>();
        let errors: ErrorSink = Arc::new(Mutex::new(Vec::new()));
        let worker_errors = Arc::clone(&errors);
        let worker = thread::Builder::new()
            .name("xgraph-cozo-writer".to_string())
            .spawn(move || writer_loop(store, receiver, worker_errors))
            .map_err(|err| {
                CozoError::Engine(format!("failed to spawn cozo writer thread: {err}"))
            })?;
        Ok(WriterHandle {
            sender: Some(sender),
            worker: Some(worker),
            errors,
        })
    }
}

/// Maximum number of `FileUpdate`s coalesced into a single Cozo
/// transaction. Larger batches amortize the per-transaction overhead
/// (one RocksDB log entry + one fsync instead of N) but increase memory
/// pressure and crash-recovery work. 64 is a balance: at our typical
/// per-file edge/node sizes this stays well under the Cozo single-script
/// param size limits and saturates write throughput in benchmarks.
const WRITER_BATCH_MAX: usize = 64;

fn writer_loop(
    store: CozoStore,
    receiver: crossbeam_channel::Receiver<WriterMessage>,
    errors: ErrorSink,
) {
    // Each outer iteration drains up to `WRITER_BATCH_MAX` queued updates
    // and commits them in a single transaction. A burst of incoming
    // updates (e.g. during initial indexing) naturally batches; a
    // steady-state trickle (one watcher event) still commits within a
    // single tx round because `recv()` blocks until any message arrives.
    loop {
        let mut batch: Vec<Box<FileUpdate>> = Vec::new();
        let mut pending_flushes: Vec<crossbeam_channel::Sender<()>> = Vec::new();
        let mut shutdown_pending = false;

        // Block until the first message.
        match receiver.recv() {
            Ok(WriterMessage::Update(first)) => batch.push(first),
            Ok(WriterMessage::Truncate(ack)) => {
                let _ = ack.send(store.truncate_graph().map_err(WriterError::Apply));
                continue;
            }
            Ok(WriterMessage::Flush(ack)) => {
                // No work to do; ack immediately and continue waiting.
                let _ = ack.send(());
                continue;
            }
            Ok(WriterMessage::Shutdown) => break,
            Err(_) => break,
        }
        // Greedily drain additional messages without blocking.
        let mut pending_truncate: Option<crossbeam_channel::Sender<Result<(), WriterError>>> = None;
        while batch.len() < WRITER_BATCH_MAX {
            match receiver.try_recv() {
                Ok(WriterMessage::Update(u)) => batch.push(u),
                Ok(WriterMessage::Truncate(ack)) => {
                    pending_truncate = Some(ack);
                    break;
                }
                Ok(WriterMessage::Flush(ack)) => {
                    // Defer the ack until the in-flight batch commits so
                    // the caller sees "fully drained" semantics.
                    pending_flushes.push(ack);
                }
                Ok(WriterMessage::Shutdown) => {
                    shutdown_pending = true;
                    break;
                }
                Err(_) => break,
            }
        }
        if let Err(err) = apply_file_updates_batch(&store, &batch) {
            let recorded = WriterError::Apply(err);
            match errors.lock() {
                Ok(mut guard) => guard.push(recorded),
                Err(poisoned) => poisoned.into_inner().push(recorded),
            }
        }
        // After the batch commits, release any flush callers waiting on
        // it — they're guaranteed all prior submissions are durable.
        for ack in pending_flushes {
            let _ = ack.send(());
        }
        if let Some(ack) = pending_truncate {
            let _ = ack.send(store.truncate_graph().map_err(WriterError::Apply));
        }
        if shutdown_pending {
            break;
        }
    }
}

/// Commit a batch of file updates inside one Cozo transaction. All
/// updates either land together or roll back together — atomicity is
/// preserved even across files. Empty batches are a no-op (the writer
/// loop never produces one, but the public surface stays robust).
///
/// Performance: a single RocksDB write batch + fsync replaces N
/// per-file commits. At our 500-file fixture this drops the store
/// phase from ~13 ms → low milliseconds (see the `phase_store_*`
/// benchmarks).
fn apply_file_updates_batch(
    store: &CozoStore,
    updates: &[Box<FileUpdate>],
) -> Result<(), CozoError> {
    if updates.is_empty() {
        return Ok(());
    }
    // Dedupe by path — within a single batch, the highest-`generation`
    // update wins. Earlier updates for the same path are obsolete (the
    // later one fully supersedes them in the database, including the
    // active_file row and every node/edge keyed off the new content
    // hash). Without this, two rapid-fire updates for the same file
    // would both insert active_node rows since the cleanup phase only
    // touches pre-existing rows, not rows being inserted in this same
    // transaction.
    let mut latest_by_path: HashMap<&str, &FileUpdate> = HashMap::new();
    for boxed in updates {
        let entry = latest_by_path.entry(boxed.path.as_str());
        match entry {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(boxed.as_ref());
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if boxed.generation > o.get().generation {
                    o.insert(boxed.as_ref());
                }
            }
        }
    }
    let updates: Vec<&FileUpdate> = latest_by_path.into_values().collect();

    let mut params = BTreeMap::new();

    // `$paths` is the set of paths whose old rows we strip out. Inline
    // relations defined in each `:rm` block iterate this set.
    let paths: Vec<DataValue> = updates
        .iter()
        .map(|u| DataValue::List(vec![DataValue::from(u.path.as_str())]))
        .collect();
    params.insert("paths".to_string(), DataValue::List(paths));

    // content_file is keyed by (content_hash) so multiple files sharing
    // a hash produce one row; the per-update row format is
    // [hash, language, parser_version, diagnostics].
    let content_files: Vec<DataValue> = updates
        .iter()
        .map(|u| -> Result<DataValue, CozoError> {
            Ok(DataValue::List(vec![
                DataValue::Bytes(u.content_hash.to_vec()),
                DataValue::from(u.language.as_str()),
                DataValue::from(i64::from(u.parser_version)),
                DataValue::Json(JsonData(serde_json::to_value(&u.diagnostics)?)),
            ]))
        })
        .collect::<Result<Vec<_>, CozoError>>()?;
    params.insert("content_files".to_string(), DataValue::List(content_files));

    // active_file is keyed by (path); one row per file.
    let active_files: Vec<DataValue> = updates
        .iter()
        .map(|u| -> Result<DataValue, CozoError> {
            Ok(DataValue::List(vec![
                DataValue::from(u.path.as_str()),
                DataValue::Bytes(u.content_hash.to_vec()),
                DataValue::from(u.mtime),
                DataValue::from(i64_from_u64(u.size)?),
                DataValue::from(i64_from_u64(u.generation)?),
            ]))
        })
        .collect::<Result<Vec<_>, CozoError>>()?;
    params.insert("active_files".to_string(), DataValue::List(active_files));

    // The remaining tables are already "list of rows" shaped; we just
    // concatenate the per-file lists into one global list. The helpers
    // emit `DataValue::List(Vec<DataValue::List(...)>)`, so we destructure
    // and re-build the outer list.
    let content_nodes = concat_rows(
        updates
            .iter()
            .map(|u| nodes_to_data_value(&u.content_hash, &u.nodes)),
    );
    params.insert("content_nodes".to_string(), content_nodes);

    let content_refs = concat_rows(
        updates
            .iter()
            .map(|u| refs_to_data_value(&u.content_hash, &u.refs)),
    );
    params.insert("content_refs".to_string(), content_refs);

    let active_nodes = concat_rows(
        updates
            .iter()
            .map(|u| active_nodes_to_data_value(&u.path, &u.content_hash, &u.nodes)),
    );
    params.insert("active_nodes".to_string(), active_nodes);

    let edges = concat_rows(updates.iter().map(|u| edges_to_data_value(&u.edges)));
    params.insert("edges".to_string(), edges);

    let symbols = concat_rows(
        updates
            .iter()
            .map(|u| symbols_to_data_value(&u.path, &u.content_hash, &u.nodes)),
    );
    params.insert("symbols".to_string(), symbols);

    store.run_mutable(BATCH_TRANSACTION_SCRIPT, params)?;
    Ok(())
}

/// Concatenate `DataValue::List(Vec<row>)` produced by each per-file
/// helper into a single `DataValue::List` so we can pass it as one Cozo
/// parameter.
fn concat_rows<I: Iterator<Item = DataValue>>(iter: I) -> DataValue {
    let mut all: Vec<DataValue> = Vec::new();
    for v in iter {
        if let DataValue::List(rows) = v {
            all.extend(rows);
        }
    }
    DataValue::List(all)
}

/// The full file-replacement transaction.
/// Batched file-replacement transaction.
///
/// Cozo runs a multi-block script as one transaction where every later block
/// sees the writes of every earlier block. Removals must therefore happen in
/// dependency order: edges depend on `active_node` rows, so edge removal
/// must precede `active_node` removal.
///
/// Cleanup phase iterates the inline `paths_to_replace` relation
/// (sourced from `$paths`, a list of `[path]` singleton rows) so old
/// edges / symbols / active_nodes for every path in the batch are
/// removed before the new rows are inserted. Lookups go through the
/// `active_node:by_path` and `symbol:by_path` secondary indexes, so the
/// cost is `O(matching_rows)` rather than a full table scan per batch.
///
/// The `:put` phase consumes already-batched parameter arrays
/// (`$content_files`, `$active_files`, `$content_nodes`, `$content_refs`,
/// `$active_nodes`, `$edges`, `$symbols`) populated by
/// `apply_file_updates_batch`.
const BATCH_TRANSACTION_SCRIPT: &str = "\
{
    paths_to_replace[path] <- $paths
    ?[source_node_id, kind, target_node_id] := paths_to_replace[p],
                                                *active_node:by_path[p, node_id],
                                                *edge[source_node_id, kind, target_node_id, _pv, _c],
                                                source_node_id = node_id
    :rm edge {source_node_id, kind, target_node_id}
}
{
    paths_to_replace[path] <- $paths
    ?[source_node_id, kind, target_node_id] := paths_to_replace[p],
                                                *active_node:by_path[p, node_id],
                                                *edge[source_node_id, kind, target_node_id, _pv, _c],
                                                target_node_id = node_id
    :rm edge {source_node_id, kind, target_node_id}
}
{
    paths_to_replace[path] <- $paths
    ?[name, kind, node_id] := paths_to_replace[p],
                              *symbol:by_path[p, name, kind, node_id]
    :rm symbol {name, kind, node_id}
}
{
    paths_to_replace[path] <- $paths
    ?[node_id] := paths_to_replace[p],
                  *active_node:by_path[p, node_id]
    :rm active_node {node_id}
}
{
    ?[content_hash, language, parser_version, diagnostics] <- $content_files
    :put content_file {content_hash => language, parser_version, diagnostics}
}
{
    ?[content_hash, local_node_id, kind, name, qname, span] <- $content_nodes
    :put content_node {content_hash, local_node_id => kind, name, qname, span}
}
{
    ?[content_hash, local_ref_id, kind, name, span] <- $content_refs
    :put content_ref {content_hash, local_ref_id => kind, name, span}
}
{
    ?[path, content_hash, mtime, size, generation] <- $active_files
    :put active_file {path => content_hash, mtime, size, generation}
}
{
    ?[node_id, path, content_hash, local_node_id, kind, name, qname, span] <- $active_nodes
    :put active_node {node_id => path, content_hash, local_node_id, kind, name, qname, span}
}
{
    ?[source_node_id, kind, target_node_id, provenance, confidence] <- $edges
    :put edge {source_node_id, kind, target_node_id => provenance, confidence}
}
{
    ?[name, kind, node_id, qname, path] <- $symbols
    :put symbol {name, kind, node_id => qname, path}
}
";

fn i64_from_u64(value: u64) -> Result<i64, CozoError> {
    i64::try_from(value).map_err(|_| {
        CozoError::Engine(format!(
            "value {value} does not fit in i64 for Cozo storage"
        ))
    })
}

fn nodes_to_data_value(hash: &ContentHash, nodes: &[NodeFact]) -> DataValue {
    let rows = nodes
        .iter()
        .map(|node| {
            DataValue::List(vec![
                DataValue::Bytes(hash.to_vec()),
                DataValue::from(i64::from(node.local_node_id)),
                DataValue::from(node.kind.as_str()),
                DataValue::from(node.name.as_str()),
                DataValue::from(node.qname.as_str()),
                node.span.to_data_value(),
            ])
        })
        .collect();
    DataValue::List(rows)
}

fn refs_to_data_value(hash: &ContentHash, refs: &[RefFact]) -> DataValue {
    let rows = refs
        .iter()
        .map(|r| {
            DataValue::List(vec![
                DataValue::Bytes(hash.to_vec()),
                DataValue::from(i64::from(r.local_ref_id)),
                DataValue::from(r.kind.as_str()),
                DataValue::from(r.name.as_str()),
                r.span.to_data_value(),
            ])
        })
        .collect();
    DataValue::List(rows)
}

fn active_nodes_to_data_value(path: &str, hash: &ContentHash, nodes: &[NodeFact]) -> DataValue {
    let rows = nodes
        .iter()
        .map(|node| {
            let node_id = active_node_id(hash, node.local_node_id);
            DataValue::List(vec![
                DataValue::from(node_id.as_str()),
                DataValue::from(path),
                DataValue::Bytes(hash.to_vec()),
                DataValue::from(i64::from(node.local_node_id)),
                DataValue::from(node.kind.as_str()),
                DataValue::from(node.name.as_str()),
                DataValue::from(node.qname.as_str()),
                node.span.to_data_value(),
            ])
        })
        .collect();
    DataValue::List(rows)
}

fn edges_to_data_value(edges: &[EdgeFact]) -> DataValue {
    let rows = edges
        .iter()
        .map(|edge| {
            DataValue::List(vec![
                DataValue::from(edge.source_node_id.as_str()),
                DataValue::from(edge.kind.as_str()),
                DataValue::from(edge.target_node_id.as_str()),
                DataValue::from(edge.provenance.as_str()),
                DataValue::from(i64::from(edge.confidence)),
            ])
        })
        .collect();
    DataValue::List(rows)
}

fn symbols_to_data_value(path: &str, hash: &ContentHash, nodes: &[NodeFact]) -> DataValue {
    let rows = nodes
        .iter()
        .map(|node| {
            let node_id = active_node_id(hash, node.local_node_id);
            DataValue::List(vec![
                DataValue::from(node.name.as_str()),
                DataValue::from(node.kind.as_str()),
                DataValue::from(node_id.as_str()),
                DataValue::from(node.qname.as_str()),
                DataValue::from(path),
            ])
        })
        .collect();
    DataValue::List(rows)
}

/// Decode JSON-typed `DataValue::Json` back into a typed payload.
pub fn decode_json<T: serde::de::DeserializeOwned>(value: DataValue) -> Result<T, CozoError> {
    match value {
        DataValue::Json(JsonData(json)) => serde_json::from_value(json).map_err(CozoError::Json),
        DataValue::Str(s) => serde_json::from_str(&s).map_err(CozoError::Json),
        other => {
            // Round-trip via JsonValue as a last resort.
            let json: JsonValue = serde_json::to_value(&other)?;
            serde_json::from_value(json).map_err(CozoError::Json)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    fn open_store() -> (CozoStore, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.cozo");
        let store = CozoStore::open(&path).expect("open store");
        (store, dir)
    }

    fn make_hash(seed: u8) -> ContentHash {
        let mut bytes = [0u8; CONTENT_HASH_LEN];
        for (i, slot) in bytes.iter_mut().enumerate() {
            *slot = seed.wrapping_add(i as u8);
        }
        ContentHash(bytes)
    }

    fn sample_update(path: &str, hash: ContentHash, generation: u64) -> FileUpdate {
        FileUpdate {
            path: path.to_string(),
            content_hash: hash,
            language: "rust".to_string(),
            parser_version: 1,
            mtime: 1_000_000,
            size: 42,
            generation,
            diagnostics: vec![Diagnostic {
                severity: "warning".to_string(),
                message: "example".to_string(),
                span: None,
            }],
            nodes: vec![NodeFact {
                local_node_id: 1,
                kind: "function".to_string(),
                name: "main".to_string(),
                qname: "crate::main".to_string(),
                span: Span {
                    start_byte: 0,
                    end_byte: 10,
                    start_row: 0,
                    start_col: 0,
                },
            }],
            refs: vec![RefFact {
                local_ref_id: 1,
                kind: "call".to_string(),
                name: "println".to_string(),
                span: Span {
                    start_byte: 4,
                    end_byte: 11,
                    start_row: 0,
                    start_col: 4,
                },
            }],
            edges: vec![],
        }
    }

    #[test]
    fn open_installs_all_relations() {
        let (store, _dir) = open_store();
        let relations = store.list_relations().expect("list relations");
        for expected in [
            "active_file",
            "active_node",
            "content_file",
            "content_node",
            "content_ref",
            "edge",
            "schema_version",
            "symbol",
        ] {
            assert!(relations.contains(expected), "missing relation {expected}");
        }
    }

    #[test]
    fn schema_version_persists_across_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.cozo");
        {
            let store = CozoStore::open(&path).expect("open store");
            assert_eq!(
                store.schema_version().expect("schema_version"),
                SCHEMA_VERSION
            );
        }
        let store = CozoStore::open(&path).expect("reopen store");
        assert_eq!(
            store.schema_version().expect("schema_version"),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn writer_queue_applies_file_update() {
        let (store, _dir) = open_store();
        let reader = store.clone();
        let mut handle = WriterQueue::start(store).expect("start writer");

        let hash = make_hash(7);
        let update = sample_update("src/main.rs", hash, 1);
        handle.submit(update.clone()).expect("submit");
        handle.shutdown();

        let rows = reader
            .run_immutable(
                "?[path, mtime, size, generation] := *active_file[path, _hash, mtime, size, generation]",
                BTreeMap::new(),
            )
            .expect("read active_file");
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0][0], DataValue::from(update.path.as_str()));
        assert_eq!(rows.rows[0][1], DataValue::from(update.mtime));
        assert_eq!(rows.rows[0][2], DataValue::from(update.size as i64));
        assert_eq!(rows.rows[0][3], DataValue::from(update.generation as i64));

        let active_nodes = reader
            .run_immutable(
                "?[node_id, kind, name] := *active_node[node_id, _p, _h, _ln, kind, name, _q, _s]",
                BTreeMap::new(),
            )
            .expect("read active_node");
        assert_eq!(active_nodes.rows.len(), 1);
        let expected_node_id = active_node_id(&hash, 1);
        assert_eq!(
            active_nodes.rows[0][0],
            DataValue::from(expected_node_id.as_str())
        );
        assert_eq!(active_nodes.rows[0][1], DataValue::from("function"));
        assert_eq!(active_nodes.rows[0][2], DataValue::from("main"));

        let content_files = reader
            .run_immutable(
                "?[language, parser_version] := *content_file[_h, language, parser_version, _d]",
                BTreeMap::new(),
            )
            .expect("read content_file");
        assert_eq!(content_files.rows.len(), 1);
        assert_eq!(content_files.rows[0][0], DataValue::from("rust"));
        assert_eq!(content_files.rows[0][1], DataValue::from(1i64));

        let symbols = reader
            .run_immutable(
                "?[name, kind, qname, path] := *symbol[name, kind, _node_id, qname, path]",
                BTreeMap::new(),
            )
            .expect("read symbols");
        assert_eq!(symbols.rows.len(), 1);
        assert_eq!(symbols.rows[0][0], DataValue::from("main"));
        assert_eq!(symbols.rows[0][1], DataValue::from("function"));
        assert_eq!(symbols.rows[0][2], DataValue::from("crate::main"));
        assert_eq!(symbols.rows[0][3], DataValue::from("src/main.rs"));
    }

    #[test]
    fn writer_queue_replaces_active_rows_for_path() {
        let (store, _dir) = open_store();
        let reader = store.clone();
        let mut handle = WriterQueue::start(store).expect("start writer");

        let first_hash = make_hash(1);
        let mut first = sample_update("src/main.rs", first_hash, 1);
        first.nodes.push(NodeFact {
            local_node_id: 2,
            kind: "function".to_string(),
            name: "helper".to_string(),
            qname: "crate::helper".to_string(),
            span: Span {
                start_byte: 20,
                end_byte: 30,
                start_row: 1,
                start_col: 0,
            },
        });
        handle.submit(first).expect("submit first");

        let second_hash = make_hash(2);
        let second = sample_update("src/main.rs", second_hash, 2);
        handle.submit(second.clone()).expect("submit second");
        handle.shutdown();

        let active_nodes = reader
            .run_immutable(
                "?[node_id] := *active_node[node_id, path, _h, _ln, _k, _n, _q, _s], path = 'src/main.rs'",
                BTreeMap::new(),
            )
            .expect("read active_node");
        assert_eq!(active_nodes.rows.len(), 1);
        let expected = active_node_id(&second_hash, 1);
        assert_eq!(active_nodes.rows[0][0], DataValue::from(expected.as_str()));

        // The first hash's content_node rows are kept (content cache); only
        // active rows for the path are replaced.
        let content_nodes = reader
            .run_immutable(
                "?[c, ln] := *content_node[c, ln, _k, _n, _q, _s]",
                BTreeMap::new(),
            )
            .expect("read content_node");
        assert!(!content_nodes.rows.is_empty());
    }

    #[test]
    fn writer_queue_handles_file_with_no_facts() {
        let (store, _dir) = open_store();
        let reader = store.clone();
        let mut handle = WriterQueue::start(store).expect("start writer");

        // A real file may have zero extractable nodes/refs/edges (e.g. an
        // empty file or one with only comments). The transaction must still
        // commit and the active_file row must land.
        let mut update = sample_update("src/empty.rs", make_hash(50), 1);
        update.nodes.clear();
        update.refs.clear();
        update.edges.clear();
        update.diagnostics.clear();
        handle.submit(update.clone()).expect("submit");
        handle.shutdown();

        let rows = reader
            .run_immutable(
                "?[path] := *active_file[path, _h, _m, _s, _g], path = 'src/empty.rs'",
                BTreeMap::new(),
            )
            .expect("read active_file");
        assert_eq!(rows.rows.len(), 1);

        let nodes = reader
            .run_immutable(
                "?[n] := *active_node[n, path, _h, _ln, _k, _na, _q, _s], path = 'src/empty.rs'",
                BTreeMap::new(),
            )
            .expect("read active_node");
        assert!(nodes.rows.is_empty());
    }

    #[test]
    fn writer_queue_removes_edges_tied_to_old_active_nodes() {
        let (store, _dir) = open_store();
        let reader = store.clone();
        let mut handle = WriterQueue::start(store).expect("start writer");

        // First update: one node and one self-edge attached to that node.
        let first_hash = make_hash(11);
        let mut first = sample_update("src/main.rs", first_hash, 1);
        let first_node_id = active_node_id(&first_hash, 1);
        first.edges.push(EdgeFact {
            source_node_id: first_node_id.clone(),
            kind: "calls".to_string(),
            target_node_id: first_node_id.clone(),
            provenance: "parser".to_string(),
            confidence: 100,
        });
        handle.submit(first).expect("submit first");

        // Second update: a fresh content hash, so the previous active node is
        // gone. The edge transaction MUST remove the orphaned edge as part of
        // the same write — otherwise stale edges point at deleted nodes.
        let second_hash = make_hash(22);
        let second = sample_update("src/main.rs", second_hash, 2);
        handle.submit(second).expect("submit second");
        handle.shutdown();

        let edges = reader
            .run_immutable("?[s, k, t] := *edge[s, k, t, _p, _c], s = $first", {
                let mut p = BTreeMap::new();
                p.insert("first".to_string(), DataValue::from(first_node_id.as_str()));
                p
            })
            .expect("read edges");
        assert!(
            edges.rows.is_empty(),
            "edge tied to old active_node was not removed (rows: {})",
            edges.rows.len()
        );
    }

    #[test]
    fn writer_handle_records_transaction_errors() {
        let (store, _dir) = open_store();
        let handle = WriterQueue::start(store).expect("start writer");

        // Submit an update whose `size` overflows i64 so apply_file_update
        // returns an Err. The worker keeps running but records the error.
        let mut update = sample_update("src/main.rs", make_hash(31), 1);
        update.size = u64::MAX;
        handle.submit(update).expect("submit");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let errors = handle.take_errors();
            if !errors.is_empty() {
                assert!(matches!(errors[0], WriterError::Apply(_)));
                return;
            }
            if Instant::now() > deadline {
                panic!("writer did not record an error within the timeout");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn readers_never_observe_half_updated_state() {
        let (store, _dir) = open_store();
        let reader = store.clone();
        let mut handle = WriterQueue::start(store).expect("start writer");

        // Prime the store with an initial row so the reader always finds one.
        let initial = sample_update("src/main.rs", make_hash(0), 0);
        handle.submit(initial).expect("submit initial");
        wait_for_active_file(&reader, "src/main.rs");

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader_thread = thread::spawn(move || {
            let mut observed = 0u64;
            while !reader_stop.load(Ordering::Relaxed) {
                let rows = reader
                    .run_immutable(
                        "?[generation] := *active_file[path, _h, _m, _s, generation], path = 'src/main.rs'",
                        BTreeMap::new(),
                    )
                    .expect("read active_file");
                assert_eq!(
                    rows.rows.len(),
                    1,
                    "active_file row vanished mid-update (observed {observed} reads)"
                );
                observed += 1;
            }
            observed
        });

        let updates = 200u64;
        for generation in 1..=updates {
            let hash = make_hash((generation % 256) as u8);
            let update = sample_update("src/main.rs", hash, generation);
            handle.submit(update).expect("submit");
        }
        handle.shutdown();

        stop.store(true, Ordering::Relaxed);
        let observed = reader_thread.join().expect("reader thread");
        assert!(
            observed > 0,
            "reader thread did not observe any reads while the writer ran"
        );
    }

    fn wait_for_active_file(store: &CozoStore, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let script =
            format!("?[generation] := *active_file[p, _h, _m, _s, generation], p = '{path}'");
        while Instant::now() < deadline {
            if let Ok(rows) = store.run_immutable(&script, BTreeMap::new())
                && !rows.rows.is_empty()
            {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("active_file for {path} never appeared");
    }

    #[test]
    fn shutdown_drains_pending_submissions() {
        let (store, _dir) = open_store();
        let reader = store.clone();
        let mut handle = WriterQueue::start(store).expect("start writer");

        // Submit a burst and immediately shut down. All submissions must land
        // because shutdown drains before joining.
        let total = 20u64;
        for generation in 1..=total {
            let hash = make_hash(generation as u8);
            let update = sample_update("src/main.rs", hash, generation);
            handle.submit(update).expect("submit");
        }
        handle.shutdown();

        let rows = reader
            .run_immutable(
                "?[generation] := *active_file[path, _h, _m, _s, generation], path = 'src/main.rs'",
                BTreeMap::new(),
            )
            .expect("read active_file");
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0][0], DataValue::from(total as i64));
    }

    #[test]
    fn submit_after_shutdown_returns_queue_closed() {
        let (store, _dir) = open_store();
        let mut handle = WriterQueue::start(store).expect("start writer");
        handle.shutdown();
        let update = sample_update("src/main.rs", make_hash(1), 1);
        let err = handle.submit(update).expect_err("expected queue closed");
        assert!(matches!(err, WriterError::QueueClosed));
    }

    #[test]
    fn diagnostics_round_trip_via_json() {
        let (store, _dir) = open_store();
        let reader = store.clone();
        let mut handle = WriterQueue::start(store).expect("start writer");

        let hash = make_hash(9);
        let mut update = sample_update("src/main.rs", hash, 1);
        update.diagnostics = vec![
            Diagnostic {
                severity: "error".to_string(),
                message: "missing semicolon".to_string(),
                span: Some(Span {
                    start_byte: 10,
                    end_byte: 11,
                    start_row: 2,
                    start_col: 4,
                }),
            },
            Diagnostic {
                severity: "warning".to_string(),
                message: "unused variable".to_string(),
                span: None,
            },
        ];
        handle.submit(update.clone()).expect("submit");
        handle.shutdown();

        let rows = reader
            .run_immutable(
                "?[diagnostics] := *content_file[_h, _l, _pv, diagnostics]",
                BTreeMap::new(),
            )
            .expect("read content_file");
        assert_eq!(rows.rows.len(), 1);
        let decoded: Vec<Diagnostic> =
            decode_json(rows.rows[0][0].clone()).expect("decode diagnostics");
        assert_eq!(decoded, update.diagnostics);
    }

    #[test]
    fn active_node_id_is_deterministic_hex() {
        let hash = ContentHash([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ]);
        let id = active_node_id(&hash, 42);
        assert_eq!(
            id,
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff:42"
        );
    }
}
