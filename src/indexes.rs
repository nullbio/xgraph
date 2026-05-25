//! Daemon-owned hot indexes for common MCP reads.
//!
//! Hot MCP calls (node by id, callers, callees, files, simple symbol lookup)
//! must not route through Cozo Datalog. They hit these in-memory structures
//! instead. Loading them from Cozo at daemon startup belongs to the daemon
//! integration phase; this module ships only the data structures and the
//! thread-safe access surface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dashmap::DashMap;
use parking_lot::RwLock;

pub use crate::resolve::NodeId;

/// Snapshot record for an active node held in the hot index.
///
/// `kind` is stored as `u32` because the `NodeKind` enum lives in another
/// module; this index intentionally does not depend on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub id: NodeId,
    pub path: PathBuf,
    pub kind: u32,
    pub name: String,
    pub qname: String,
    pub span_start: u32,
    pub span_end: u32,
}

/// Key used to find candidate node ids for a name + kind pair.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SymbolKey {
    pub name: String,
    pub kind: u32,
}

/// In-memory indexes owned by the daemon.
///
/// All operations are safe to call from multiple threads. Returned vectors
/// are owned copies so callers never observe internal locks or shards.
///
/// `nodes_in_file` returns node ids in the order they were supplied to
/// `insert_file`. `callers_of` / `callees_of` return node ids in insertion
/// order; duplicate edges are not stored.
pub struct HotIndexes {
    nodes: DashMap<NodeId, NodeRecord>,
    symbols: DashMap<SymbolKey, Vec<NodeId>>,
    files: RwLock<HashMap<PathBuf, Vec<NodeId>>>,
    callers: RwLock<HashMap<NodeId, Vec<NodeId>>>,
    callees: RwLock<HashMap<NodeId, Vec<NodeId>>>,
}

impl HotIndexes {
    pub fn new() -> Self {
        Self {
            nodes: DashMap::new(),
            symbols: DashMap::new(),
            files: RwLock::new(HashMap::new()),
            callers: RwLock::new(HashMap::new()),
            callees: RwLock::new(HashMap::new()),
        }
    }

    pub fn insert_node(&self, record: NodeRecord) {
        self.nodes.insert(record.id, record);
    }

    pub fn get_node(&self, id: NodeId) -> Option<NodeRecord> {
        self.nodes.get(&id).map(|entry| entry.value().clone())
    }

    pub fn remove_node(&self, id: NodeId) -> Option<NodeRecord> {
        self.nodes.remove(&id).map(|(_, record)| record)
    }

    pub fn insert_file(&self, path: PathBuf, node_ids: Vec<NodeId>) {
        self.files.write().insert(path, node_ids);
    }

    pub fn nodes_in_file(&self, path: &Path) -> Vec<NodeId> {
        self.files.read().get(path).cloned().unwrap_or_default()
    }

    pub fn remove_file(&self, path: &Path) -> Vec<NodeId> {
        self.files.write().remove(path).unwrap_or_default()
    }

    pub fn add_call_edge(&self, caller: NodeId, callee: NodeId) {
        {
            let mut callees = self.callees.write();
            let targets = callees.entry(caller).or_default();
            if !targets.contains(&callee) {
                targets.push(callee);
            }
        }
        {
            let mut callers = self.callers.write();
            let sources = callers.entry(callee).or_default();
            if !sources.contains(&caller) {
                sources.push(caller);
            }
        }
    }

    pub fn remove_call_edge(&self, caller: NodeId, callee: NodeId) {
        {
            let mut callees = self.callees.write();
            if let Some(targets) = callees.get_mut(&caller) {
                targets.retain(|id| *id != callee);
                if targets.is_empty() {
                    callees.remove(&caller);
                }
            }
        }
        {
            let mut callers = self.callers.write();
            if let Some(sources) = callers.get_mut(&callee) {
                sources.retain(|id| *id != caller);
                if sources.is_empty() {
                    callers.remove(&callee);
                }
            }
        }
    }

    pub fn callers_of(&self, callee: NodeId) -> Vec<NodeId> {
        self.callers
            .read()
            .get(&callee)
            .cloned()
            .unwrap_or_default()
    }

    pub fn callees_of(&self, caller: NodeId) -> Vec<NodeId> {
        self.callees
            .read()
            .get(&caller)
            .cloned()
            .unwrap_or_default()
    }

    pub fn register_symbol(&self, key: SymbolKey, node_id: NodeId) {
        let mut entry = self.symbols.entry(key).or_default();
        if !entry.contains(&node_id) {
            entry.push(node_id);
        }
    }

    pub fn lookup_symbol(&self, key: &SymbolKey) -> Vec<NodeId> {
        self.symbols
            .get(key)
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }

    pub fn unregister_symbol(&self, key: &SymbolKey, node_id: NodeId) {
        self.symbols.remove_if_mut(key, |_, ids| {
            ids.retain(|id| *id != node_id);
            ids.is_empty()
        });
    }
}

impl Default for HotIndexes {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    fn sample_record(id: u64, path: &str, name: &str) -> NodeRecord {
        NodeRecord {
            id: NodeId(id),
            path: PathBuf::from(path),
            kind: 1,
            name: name.to_string(),
            qname: format!("ns::{name}"),
            span_start: 0,
            span_end: 10,
        }
    }

    #[test]
    fn node_insert_lookup_remove() {
        let indexes = HotIndexes::new();
        let record = sample_record(7, "src/a.rs", "alpha");

        indexes.insert_node(record.clone());
        assert_eq!(indexes.get_node(NodeId(7)), Some(record.clone()));

        let removed = indexes.remove_node(NodeId(7));
        assert_eq!(removed, Some(record));
        assert!(indexes.get_node(NodeId(7)).is_none());
    }

    #[test]
    fn file_insert_lookup_remove_preserves_insertion_order() {
        let indexes = HotIndexes::new();
        let path = PathBuf::from("src/a.rs");
        let ids = vec![NodeId(3), NodeId(1), NodeId(2)];

        indexes.insert_file(path.clone(), ids.clone());
        assert_eq!(indexes.nodes_in_file(&path), ids);

        let removed = indexes.remove_file(&path);
        assert_eq!(removed, ids);
        assert!(indexes.nodes_in_file(&path).is_empty());
    }

    #[test]
    fn nodes_in_file_returns_all_inserted_ids() {
        let indexes = HotIndexes::new();
        let path = PathBuf::from("src/big.rs");
        let ids: Vec<NodeId> = (0..32).map(NodeId).collect();

        indexes.insert_file(path.clone(), ids.clone());

        let retrieved = indexes.nodes_in_file(&path);
        assert_eq!(retrieved.len(), ids.len());
        assert_eq!(retrieved, ids);
    }

    #[test]
    fn call_edges_round_trip_and_dedupe() {
        let indexes = HotIndexes::new();
        let caller = NodeId(1);
        let callee = NodeId(2);

        indexes.add_call_edge(caller, callee);
        indexes.add_call_edge(caller, callee);

        assert_eq!(indexes.callees_of(caller), vec![callee]);
        assert_eq!(indexes.callers_of(callee), vec![caller]);

        indexes.remove_call_edge(caller, callee);
        assert!(indexes.callees_of(caller).is_empty());
        assert!(indexes.callers_of(callee).is_empty());
    }

    #[test]
    fn remove_call_edge_does_not_affect_other_edges() {
        let indexes = HotIndexes::new();
        let caller = NodeId(1);
        let a = NodeId(2);
        let b = NodeId(3);

        indexes.add_call_edge(caller, a);
        indexes.add_call_edge(caller, b);

        indexes.remove_call_edge(caller, a);

        assert_eq!(indexes.callees_of(caller), vec![b]);
        assert!(indexes.callers_of(a).is_empty());
        assert_eq!(indexes.callers_of(b), vec![caller]);
    }

    #[test]
    fn symbol_lookup_returns_all_registered_nodes() {
        let indexes = HotIndexes::new();
        let key = SymbolKey {
            name: "do_work".to_string(),
            kind: 2,
        };

        indexes.register_symbol(key.clone(), NodeId(10));
        indexes.register_symbol(key.clone(), NodeId(20));
        indexes.register_symbol(key.clone(), NodeId(10)); // dedupe

        let mut hits = indexes.lookup_symbol(&key);
        hits.sort();
        assert_eq!(hits, vec![NodeId(10), NodeId(20)]);

        indexes.unregister_symbol(&key, NodeId(10));
        assert_eq!(indexes.lookup_symbol(&key), vec![NodeId(20)]);

        indexes.unregister_symbol(&key, NodeId(20));
        assert!(indexes.lookup_symbol(&key).is_empty());
    }

    #[test]
    fn concurrent_node_inserts_are_all_retrievable() {
        let indexes = Arc::new(HotIndexes::new());
        let thread_count = 8u64;
        let per_thread = 500u64;

        let mut handles = Vec::with_capacity(thread_count as usize);
        for t in 0..thread_count {
            let indexes = Arc::clone(&indexes);
            handles.push(thread::spawn(move || {
                for i in 0..per_thread {
                    let id = t * per_thread + i;
                    indexes.insert_node(sample_record(id, "src/x.rs", "n"));
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }

        for t in 0..thread_count {
            for i in 0..per_thread {
                let id = t * per_thread + i;
                assert!(
                    indexes.get_node(NodeId(id)).is_some(),
                    "missing node {id} after concurrent insert"
                );
            }
        }
    }

    #[test]
    fn concurrent_call_edges_are_consistent() {
        let indexes = Arc::new(HotIndexes::new());
        let caller = NodeId(0);
        let callee_count = 2_000u64;

        let mut handles = Vec::new();
        let chunk = 250u64;
        let mut start = 1u64;
        while start <= callee_count {
            let end = (start + chunk - 1).min(callee_count);
            let indexes = Arc::clone(&indexes);
            handles.push(thread::spawn(move || {
                for id in start..=end {
                    indexes.add_call_edge(caller, NodeId(id));
                }
            }));
            start = end + 1;
        }
        for h in handles {
            h.join().expect("edge writer panicked");
        }

        let callees: HashSet<NodeId> = indexes.callees_of(caller).into_iter().collect();
        let expected: HashSet<NodeId> = (1..=callee_count).map(NodeId).collect();
        assert_eq!(callees, expected);

        for id in 1..=callee_count {
            assert_eq!(indexes.callers_of(NodeId(id)), vec![caller]);
        }
    }

    #[test]
    fn concurrent_symbol_registration_collects_all_ids() {
        let indexes = Arc::new(HotIndexes::new());
        let key = SymbolKey {
            name: "shared".to_string(),
            kind: 1,
        };

        let thread_count = 6u64;
        let per_thread = 200u64;

        let mut handles = Vec::with_capacity(thread_count as usize);
        for t in 0..thread_count {
            let indexes = Arc::clone(&indexes);
            let key = key.clone();
            handles.push(thread::spawn(move || {
                for i in 0..per_thread {
                    let id = t * per_thread + i;
                    indexes.register_symbol(key.clone(), NodeId(id));
                }
            }));
        }
        for h in handles {
            h.join().expect("symbol writer panicked");
        }

        let hits: HashSet<NodeId> = indexes.lookup_symbol(&key).into_iter().collect();
        let expected: HashSet<NodeId> = (0..thread_count * per_thread).map(NodeId).collect();
        assert_eq!(hits, expected);
    }

    #[test]
    fn default_matches_new() {
        let a = HotIndexes::default();
        let b = HotIndexes::new();
        a.insert_node(sample_record(1, "p", "n"));
        b.insert_node(sample_record(1, "p", "n"));
        assert_eq!(a.get_node(NodeId(1)), b.get_node(NodeId(1)));
    }
}
