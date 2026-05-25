//! Embedded CozoDB schema and the single-writer queue.
//!
//! This module owns the on-disk CozoDB engine and the only thread permitted to
//! mutate it. All graph mutations flow through [`WriterHandle::submit`], are
//! serialized by a dedicated worker thread, and are applied as one CozoScript
//! transaction per file update so readers never observe a half-updated state.

use std::collections::BTreeMap;
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

    fn install_schema(&self) -> Result<(), CozoError> {
        let existing = self.list_relations()?;
        for (name, script) in RELATION_DDL {
            if !existing.contains(*name) {
                self.run_mutable(script, BTreeMap::new())?;
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

fn writer_loop(
    store: CozoStore,
    receiver: crossbeam_channel::Receiver<WriterMessage>,
    errors: ErrorSink,
) {
    while let Ok(message) = receiver.recv() {
        match message {
            WriterMessage::Update(update) => {
                if let Err(err) = apply_file_update(&store, &update) {
                    let recorded = WriterError::Apply(err);
                    match errors.lock() {
                        Ok(mut guard) => guard.push(recorded),
                        Err(poisoned) => poisoned.into_inner().push(recorded),
                    }
                }
            }
            WriterMessage::Shutdown => break,
        }
    }
}

fn apply_file_update(store: &CozoStore, update: &FileUpdate) -> Result<(), CozoError> {
    let mut params = BTreeMap::new();

    params.insert("path".to_string(), DataValue::from(update.path.as_str()));
    params.insert(
        "content_hash".to_string(),
        DataValue::Bytes(update.content_hash.to_vec()),
    );
    params.insert(
        "language".to_string(),
        DataValue::from(update.language.as_str()),
    );
    params.insert(
        "parser_version".to_string(),
        DataValue::from(i64::from(update.parser_version)),
    );
    params.insert("mtime".to_string(), DataValue::from(update.mtime));
    params.insert(
        "size".to_string(),
        DataValue::from(i64_from_u64(update.size)?),
    );
    params.insert(
        "generation".to_string(),
        DataValue::from(i64_from_u64(update.generation)?),
    );
    params.insert(
        "diagnostics".to_string(),
        DataValue::Json(JsonData(serde_json::to_value(&update.diagnostics)?)),
    );

    let content_nodes = nodes_to_data_value(&update.content_hash, &update.nodes);
    params.insert("content_nodes".to_string(), content_nodes);

    let content_refs = refs_to_data_value(&update.content_hash, &update.refs);
    params.insert("content_refs".to_string(), content_refs);

    let active_nodes =
        active_nodes_to_data_value(&update.path, &update.content_hash, &update.nodes);
    params.insert("active_nodes".to_string(), active_nodes);

    let edges = edges_to_data_value(&update.edges);
    params.insert("edges".to_string(), edges);

    let symbols = symbols_to_data_value(&update.path, &update.content_hash, &update.nodes);
    params.insert("symbols".to_string(), symbols);

    // A single CozoScript wrapped in multiple `{...}` blocks runs atomically:
    // every block sees the same snapshot, and either all commit or all roll
    // back. This satisfies the "one transaction per file update" invariant.
    let script = TRANSACTION_SCRIPT;

    store.run_mutable(script, params)?;
    Ok(())
}

/// The full file-replacement transaction.
///
/// Cozo runs a multi-block script as one transaction where every later block
/// sees the writes of every earlier block. Removals must therefore happen in
/// dependency order: edges depend on the path's previous `active_node` rows,
/// so edge removal must precede `active_node` removal.
///
/// Steps:
/// 1. Remove every `edge` whose source is in the path's old active nodes.
/// 2. Remove every `edge` whose target is in the path's old active nodes.
/// 3. Remove the path's old `symbol` rows.
/// 4. Remove the path's old `active_node` rows.
/// 5. Upsert the `content_file`, `content_node`, and `content_ref` rows.
///    (`:put` on a `content_*` key replaces the row, so no `:rm` is needed.)
/// 6. Upsert the new `active_file` row (`:put` again replaces in place).
/// 7. Insert the new `active_node`, `edge`, and `symbol` rows.
const TRANSACTION_SCRIPT: &str = "\
{
    ?[source_node_id, kind, target_node_id] := *active_node[node_id, path, _ch, _ln, _k, _n, _q, _s],
                                                path = $path,
                                                *edge[source_node_id, kind, target_node_id, _p, _c],
                                                source_node_id = node_id
    :rm edge {source_node_id, kind, target_node_id}
}
{
    ?[source_node_id, kind, target_node_id] := *active_node[node_id, path, _ch, _ln, _k, _n, _q, _s],
                                                path = $path,
                                                *edge[source_node_id, kind, target_node_id, _p, _c],
                                                target_node_id = node_id
    :rm edge {source_node_id, kind, target_node_id}
}
{
    ?[name, kind, node_id] := *symbol[name, kind, node_id, _qname, path], path = $path
    :rm symbol {name, kind, node_id}
}
{
    ?[node_id] := *active_node[node_id, path, _ch, _ln, _k, _n, _q, _s], path = $path
    :rm active_node {node_id}
}
{
    ?[content_hash, language, parser_version, diagnostics] <- [[$content_hash, $language, $parser_version, $diagnostics]]
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
    ?[path, content_hash, mtime, size, generation] <- [[$path, $content_hash, $mtime, $size, $generation]]
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
