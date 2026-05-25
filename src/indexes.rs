//! Daemon-owned hot indexes for common MCP reads.
//!
//! Hot MCP calls (node by id, callers, callees, files, simple symbol lookup)
//! hit these in-memory structures instead of issuing a Datalog query. The
//! indexes are populated from Cozo on daemon startup via `load_from_cozo` and
//! kept in sync incrementally as `WorktreeOwner` applies file updates via
//! `apply_file_update` / `remove_path`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cozo::DataValue;
use dashmap::DashMap;
use parking_lot::RwLock;

use crate::cozo::{CozoError, CozoStore, FileUpdate, active_node_id};

/// Stable node identifier shared with Cozo's `active_node` relation.
///
/// Wraps `Arc<str>` so handlers and mutation paths can share an id cheaply.
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub struct NodeId(pub Arc<str>);

impl NodeId {
    pub fn new(s: impl Into<Arc<str>>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

/// Snapshot record for an active node held in the hot index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub id: NodeId,
    pub path: PathBuf,
    pub kind: String,
    pub name: String,
    pub qname: String,
}

/// Key used to find candidate node ids for a name + kind pair.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SymbolKey {
    pub name: String,
    pub kind: String,
}

/// In-memory indexes owned by the daemon.
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

    /// Populate the indexes from the Cozo store. Designed for daemon startup
    /// when an empty `HotIndexes` is constructed in front of an existing
    /// persistent graph.
    pub fn load_from_cozo(store: &CozoStore) -> Result<Self, CozoError> {
        let me = Self::new();
        me.reload_from_cozo(store)?;
        Ok(me)
    }

    pub fn reload_from_cozo(&self, store: &CozoStore) -> Result<(), CozoError> {
        // active_node[node_id] => path, content_hash, local_node_id, kind, name, qname, span
        let rows = store.run_read(
            "?[node_id, path, kind, name, qname] := \
             *active_node[node_id, path, _hash, _local, kind, name, qname, _span]",
            BTreeMap::new(),
        )?;
        for row in rows.rows {
            let mut iter = row.into_iter();
            let Some(node_id) = data_to_string(iter.next()) else {
                continue;
            };
            let path = data_to_string(iter.next()).unwrap_or_default();
            let kind = data_to_string(iter.next()).unwrap_or_default();
            let name = data_to_string(iter.next()).unwrap_or_default();
            let qname = data_to_string(iter.next()).unwrap_or_default();
            let id = NodeId::from(node_id);
            let record = NodeRecord {
                id: id.clone(),
                path: PathBuf::from(&path),
                kind,
                name,
                qname,
            };
            self.nodes.insert(id.clone(), record);
            self.files
                .write()
                .entry(PathBuf::from(&path))
                .or_default()
                .push(id);
        }

        // symbol[name, kind, node_id] => qname, path
        let rows = store.run_read(
            "?[name, kind, node_id] := *symbol[name, kind, node_id, _qname, _path]",
            BTreeMap::new(),
        )?;
        for row in rows.rows {
            let mut iter = row.into_iter();
            let name = data_to_string(iter.next()).unwrap_or_default();
            let kind = data_to_string(iter.next()).unwrap_or_default();
            let Some(node_id) = data_to_string(iter.next()) else {
                continue;
            };
            self.register_symbol(SymbolKey { name, kind }, NodeId::from(node_id));
        }

        // edge[source, kind, target] => provenance, confidence — wire call edges.
        let rows = store.run_read(
            "?[source, target] := *edge[source, $kind, target, _prov, _conf]",
            [("kind".to_string(), DataValue::from("calls".to_string()))].into(),
        )?;
        for row in rows.rows {
            let mut iter = row.into_iter();
            let Some(src) = data_to_string(iter.next()) else {
                continue;
            };
            let Some(dst) = data_to_string(iter.next()) else {
                continue;
            };
            self.add_call_edge(NodeId::from(src), NodeId::from(dst));
        }
        Ok(())
    }

    /// Mirror the effect of a Cozo `FileUpdate` transaction into the indexes.
    /// Call this when `WriterHandle::submit` returns Ok; the writer thread
    /// performs the durable commit in parallel.
    pub fn apply_file_update(&self, update: &FileUpdate) {
        let path = PathBuf::from(&update.path);

        // Remove any prior active nodes for this path.
        let prior = self.files.write().remove(&path).unwrap_or_default();
        for id in &prior {
            self.nodes.remove(id);
            self.callees.write().remove(id);
            // Remove this node as a target from any callers map.
            let callers = self.callers.write().remove(id);
            if let Some(srcs) = callers {
                for src in srcs {
                    if let Some(targets) = self.callees.write().get_mut(&src) {
                        targets.retain(|t| t != id);
                    }
                }
            }
            // Drop symbol entries that reference this id.
            self.symbols.retain(|_, ids| {
                ids.retain(|x| x != id);
                !ids.is_empty()
            });
        }

        // Insert new active nodes.
        let mut new_ids: Vec<NodeId> = Vec::with_capacity(update.nodes.len());
        for node in &update.nodes {
            let global_id = NodeId::from(active_node_id(&update.content_hash, node.local_node_id));
            let record = NodeRecord {
                id: global_id.clone(),
                path: path.clone(),
                kind: node.kind.clone(),
                name: node.name.clone(),
                qname: node.qname.clone(),
            };
            self.nodes.insert(global_id.clone(), record);
            // Register by name and qname so unqualified callers also resolve.
            self.register_symbol(
                SymbolKey {
                    name: node.name.clone(),
                    kind: node.kind.clone(),
                },
                global_id.clone(),
            );
            if node.qname != node.name {
                self.register_symbol(
                    SymbolKey {
                        name: node.qname.clone(),
                        kind: node.kind.clone(),
                    },
                    global_id.clone(),
                );
            }
            new_ids.push(global_id);
        }
        if !new_ids.is_empty() {
            self.files.write().insert(path, new_ids);
        }

        // Wire in edges.
        for edge in &update.edges {
            if edge.kind == "calls" {
                self.add_call_edge(
                    NodeId::from(edge.source_node_id.clone()),
                    NodeId::from(edge.target_node_id.clone()),
                );
            }
        }
    }

    /// Drop all active state for a path (e.g., file deleted on disk).
    pub fn remove_path(&self, path: &Path) {
        let prior = self.files.write().remove(path).unwrap_or_default();
        for id in &prior {
            self.nodes.remove(id);
            self.callees.write().remove(id);
            let callers = self.callers.write().remove(id);
            if let Some(srcs) = callers {
                for src in srcs {
                    if let Some(targets) = self.callees.write().get_mut(&src) {
                        targets.retain(|t| t != id);
                    }
                }
            }
            self.symbols.retain(|_, ids| {
                ids.retain(|x| x != id);
                !ids.is_empty()
            });
        }
    }

    pub fn insert_node(&self, record: NodeRecord) {
        self.nodes.insert(record.id.clone(), record);
    }

    pub fn get_node(&self, id: &NodeId) -> Option<NodeRecord> {
        self.nodes.get(id).map(|entry| entry.value().clone())
    }

    pub fn remove_node(&self, id: &NodeId) -> Option<NodeRecord> {
        self.nodes.remove(id).map(|(_, record)| record)
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
            let targets = callees.entry(caller.clone()).or_default();
            if !targets.contains(&callee) {
                targets.push(callee.clone());
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

    pub fn remove_call_edge(&self, caller: &NodeId, callee: &NodeId) {
        {
            let mut callees = self.callees.write();
            if let Some(targets) = callees.get_mut(caller) {
                targets.retain(|id| id != callee);
                if targets.is_empty() {
                    callees.remove(caller);
                }
            }
        }
        {
            let mut callers = self.callers.write();
            if let Some(sources) = callers.get_mut(callee) {
                sources.retain(|id| id != caller);
                if sources.is_empty() {
                    callers.remove(callee);
                }
            }
        }
    }

    pub fn callers_of(&self, callee: &NodeId) -> Vec<NodeId> {
        self.callers.read().get(callee).cloned().unwrap_or_default()
    }

    pub fn callees_of(&self, caller: &NodeId) -> Vec<NodeId> {
        self.callees.read().get(caller).cloned().unwrap_or_default()
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

    /// Symbols matching `name`, regardless of `kind`. Useful for the MCP
    /// `find_symbol` call where the caller doesn't supply a kind filter.
    pub fn lookup_symbol_by_name(&self, name: &str) -> Vec<NodeId> {
        let mut hits = Vec::new();
        for entry in self.symbols.iter() {
            if entry.key().name == name {
                hits.extend(entry.value().iter().cloned());
            }
        }
        hits
    }

    pub fn unregister_symbol(&self, key: &SymbolKey, node_id: &NodeId) {
        self.symbols.remove_if_mut(key, |_, ids| {
            ids.retain(|id| id != node_id);
            ids.is_empty()
        });
    }
}

impl Default for HotIndexes {
    fn default() -> Self {
        Self::new()
    }
}

fn data_to_string(value: Option<DataValue>) -> Option<String> {
    match value? {
        DataValue::Str(s) => Some(s.into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(id: &str, path: &str, name: &str) -> NodeRecord {
        NodeRecord {
            id: NodeId::from(id),
            path: PathBuf::from(path),
            kind: "function".to_string(),
            name: name.to_string(),
            qname: name.to_string(),
        }
    }

    #[test]
    fn node_lookup_roundtrip() {
        let idx = HotIndexes::new();
        idx.insert_node(sample_record("h:1", "a.rs", "foo"));
        let got = idx.get_node(&NodeId::from("h:1")).expect("present");
        assert_eq!(got.name, "foo");
        idx.remove_node(&NodeId::from("h:1"));
        assert!(idx.get_node(&NodeId::from("h:1")).is_none());
    }

    #[test]
    fn files_index_returns_inserted_ids() {
        let idx = HotIndexes::new();
        idx.insert_file(
            PathBuf::from("foo.rs"),
            vec![NodeId::from("h:1"), NodeId::from("h:2")],
        );
        let ids = idx.nodes_in_file(Path::new("foo.rs"));
        assert_eq!(ids, vec![NodeId::from("h:1"), NodeId::from("h:2")]);
    }

    #[test]
    fn call_edges_consistent_both_directions() {
        let idx = HotIndexes::new();
        let caller = NodeId::from("h:caller");
        let callee = NodeId::from("h:callee");
        idx.add_call_edge(caller.clone(), callee.clone());
        assert_eq!(idx.callees_of(&caller), vec![callee.clone()]);
        assert_eq!(idx.callers_of(&callee), vec![caller.clone()]);
        idx.remove_call_edge(&caller, &callee);
        assert!(idx.callees_of(&caller).is_empty());
        assert!(idx.callers_of(&callee).is_empty());
    }

    #[test]
    fn symbol_register_lookup_unregister() {
        let idx = HotIndexes::new();
        let key = SymbolKey {
            name: "Foo".to_string(),
            kind: "class".to_string(),
        };
        idx.register_symbol(key.clone(), NodeId::from("h:1"));
        idx.register_symbol(key.clone(), NodeId::from("h:2"));
        assert_eq!(idx.lookup_symbol(&key).len(), 2);
        idx.unregister_symbol(&key, &NodeId::from("h:1"));
        assert_eq!(idx.lookup_symbol(&key), vec![NodeId::from("h:2")]);
    }

    #[test]
    fn lookup_by_name_filters_across_kinds() {
        let idx = HotIndexes::new();
        idx.register_symbol(
            SymbolKey {
                name: "Foo".to_string(),
                kind: "class".to_string(),
            },
            NodeId::from("h:1"),
        );
        idx.register_symbol(
            SymbolKey {
                name: "Foo".to_string(),
                kind: "function".to_string(),
            },
            NodeId::from("h:2"),
        );
        let mut hits = idx.lookup_symbol_by_name("Foo");
        hits.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(hits, vec![NodeId::from("h:1"), NodeId::from("h:2")]);
    }
}
