//! The manifest Y.Doc — v1's single source of truth for the vault
//! namespace. See `docs/DESIGN_DOC_V1.md` §2 and §3.2.
//!
//! The Yrs document has one top-level `Y.Map` named `"nodes"`. Each key
//! is a `NodeId` (UUIDv7, hyphenated) and each value is a nested
//! `Y.Map` holding the `NodeEntry` fields.
//!
//! Field layout inside an entry sub-map (all values are `Any` primitives):
//!
//! | key          | type    | meaning                                          |
//! |--------------|---------|--------------------------------------------------|
//! | `name`       | String  | last path segment                                |
//! | `parent`     | String  | parent NodeId, empty string = vault root         |
//! | `deleted`    | bool    | tombstone flag                                   |
//! | `kind`       | String  | `"text"` / `"binary"` / `"directory"`            |
//! | `blob`       | String  | hex SHA-256, absent unless `kind == "binary"`    |
//! | `size`       | i64     | bytes (hint for UI / GC)                         |
//! | `created_at` | i64     | lamport of the create event (immutable)          |
//! | `c_actor`    | String  | actor that created the entry (immutable)         |
//! | `del_lamp`   | i64     | lamport of the most recent `deleted=true` write  |
//! | `del_actor`  | String  | actor of the most recent `deleted=true` write    |
//! | `mod_lamp`   | i64     | lamport of the most recent content modification  |
//! | `mod_actor`  | String  | actor of the most recent content modification    |
//!
//! The `(del_lamp, del_actor)` and `(mod_lamp, mod_actor)` stamps are
//! compared at projection time to implement
//! **modify-wins-over-delete** (§6.3 of the design doc).

use super::ids::{ActorId, Lamport, NodeId, Stamp};
use std::collections::HashMap;
use yrs::{Any, Doc, Map, MapPrelim, MapRef, Out, ReadTxn, Transact};

/// Classification of a node. Immutable after the node is created.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Text,
    Binary,
    Directory,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Text => "text",
            NodeKind::Binary => "binary",
            NodeKind::Directory => "directory",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "text" => Some(NodeKind::Text),
            "binary" => Some(NodeKind::Binary),
            "directory" => Some(NodeKind::Directory),
            _ => None,
        }
    }
}

/// A projected view of one entry in the manifest. This is what read
/// paths and the projection code consume; never held long-term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeEntry {
    pub id: NodeId,
    pub name: String,
    pub parent: Option<NodeId>,
    pub deleted: bool,
    pub kind: NodeKind,
    pub blob_hash: Option<String>,
    pub size: u64,
    pub created_at: Lamport,
    pub created_by: ActorId,
    pub delete_stamp: Option<Stamp>,
    pub modify_stamp: Option<Stamp>,
}

impl NodeEntry {
    /// Effective lamport for LWW comparisons at projection time. Takes
    /// the max of `created_at` and any field-write stamp we've recorded.
    pub fn effective_stamp(&self) -> Stamp {
        let create = Stamp::new(self.created_at, self.created_by);
        let candidates = [Some(create), self.delete_stamp, self.modify_stamp];
        candidates.into_iter().flatten().max().unwrap()
    }
}

/// Wraps a Yrs `Doc` with a typed API for the manifest schema. Owns the
/// local actor id and lamport counter; each mutating method bumps the
/// counter and stamps the written fields.
pub struct Manifest {
    doc: Doc,
    nodes: MapRef,
    actor: ActorId,
    lamport: Lamport,
}

impl Manifest {
    /// Create an empty manifest for `actor`. Lamport starts at 0.
    pub fn new(actor: ActorId) -> Self {
        let doc = Doc::new();
        let nodes = doc.get_or_insert_map("nodes");
        Self {
            doc,
            nodes,
            actor,
            lamport: Lamport::ZERO,
        }
    }

    /// Rehydrate a manifest from a Yrs state update (as produced by
    /// `encode_state_as_update`).
    pub fn from_update(actor: ActorId, lamport: Lamport, update: &[u8]) -> anyhow::Result<Self> {
        use yrs::updates::decoder::Decode;
        let doc = Doc::new();
        let nodes = doc.get_or_insert_map("nodes");
        {
            let mut txn = doc.transact_mut();
            let update = yrs::Update::decode_v1(update)?;
            txn.apply_update(update);
        }
        Ok(Self {
            doc,
            nodes,
            actor,
            lamport,
        })
    }

    pub fn actor(&self) -> ActorId {
        self.actor
    }

    pub fn lamport(&self) -> Lamport {
        self.lamport
    }

    /// Expose the underlying Yrs doc so callers can subscribe to
    /// updates, encode state, apply remote updates, etc.
    pub fn doc(&self) -> &Doc {
        &self.doc
    }

    /// Encode the full current state for a fresh peer.
    pub fn encode_state_as_update(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    /// Apply a remote Yrs update and advance our lamport past anything
    /// the update carried. Returns `Ok(())` on success.
    pub fn apply_update(&mut self, update: &[u8]) -> anyhow::Result<()> {
        use yrs::updates::decoder::Decode;
        let update = yrs::Update::decode_v1(update)?;
        {
            let mut txn = self.doc.transact_mut();
            txn.apply_update(update);
        }
        // Scan for any lamport stamps newer than ours and advance.
        let max_seen = self.max_lamport_in_doc();
        if let Some(m) = max_seen {
            self.lamport.observe(m);
        }
        Ok(())
    }

    fn max_lamport_in_doc(&self) -> Option<Lamport> {
        let txn = self.doc.transact();
        let mut max: Option<u64> = None;
        for (_id, out) in self.nodes.iter(&txn) {
            let Out::YMap(m) = out else { continue };
            for field in ["created_at", "del_lamp", "mod_lamp"] {
                if let Some(v) = read_u64(&m, &txn, field) {
                    max = Some(max.map_or(v, |cur| cur.max(v)));
                }
            }
        }
        max.map(Lamport)
    }

    // ------------------------------------------------------------------
    // Mutating API — every method bumps the local lamport once.
    // ------------------------------------------------------------------

    /// Insert a brand-new node. Returns the chosen `NodeId`.
    pub fn create_node(
        &mut self,
        name: &str,
        parent: Option<NodeId>,
        kind: NodeKind,
        blob_hash: Option<&str>,
        size: u64,
    ) -> NodeId {
        let id = NodeId::new();
        let lamp = self.lamport.tick();
        let actor = self.actor;
        let parent_str = parent
            .map(|p| p.to_string_hyphenated())
            .unwrap_or_default();

        let entry = MapPrelim::from([
            ("name", Any::from(name.to_string())),
            ("parent", Any::from(parent_str)),
            ("deleted", Any::from(false)),
            ("kind", Any::from(kind.as_str().to_string())),
            (
                "blob",
                Any::from(blob_hash.unwrap_or("").to_string()),
            ),
            ("size", Any::from(size as i64)),
            ("created_at", Any::from(lamp.get() as i64)),
            ("c_actor", Any::from(actor.to_string_hyphenated())),
        ]);

        let mut txn = self.doc.transact_mut();
        self.nodes.insert(&mut txn, id.to_string_hyphenated(), entry);
        id
    }

    /// Rename: update the `name` field on an existing node.
    /// No-op if the node does not exist.
    pub fn set_name(&mut self, id: NodeId, new_name: &str) -> bool {
        let lamp = self.lamport.tick();
        let actor = self.actor;
        let actor_str = actor.to_string_hyphenated();
        let mut txn = self.doc.transact_mut();
        let Some(entry) = get_entry_map(&self.nodes, &txn, id) else {
            return false;
        };
        entry.insert(&mut txn, "name", new_name.to_string());
        entry.insert(&mut txn, "mod_lamp", lamp.get() as i64);
        entry.insert(&mut txn, "mod_actor", actor_str);
        true
    }

    /// Move: update the `parent` field on an existing node.
    pub fn set_parent(&mut self, id: NodeId, new_parent: Option<NodeId>) -> bool {
        let lamp = self.lamport.tick();
        let actor_str = self.actor.to_string_hyphenated();
        let parent_str = new_parent
            .map(|p| p.to_string_hyphenated())
            .unwrap_or_default();
        let mut txn = self.doc.transact_mut();
        let Some(entry) = get_entry_map(&self.nodes, &txn, id) else {
            return false;
        };
        entry.insert(&mut txn, "parent", parent_str);
        entry.insert(&mut txn, "mod_lamp", lamp.get() as i64);
        entry.insert(&mut txn, "mod_actor", actor_str);
        true
    }

    /// Set `deleted=true`. Records (del_lamp, del_actor) for
    /// modify-wins-over-delete comparisons at projection time.
    pub fn delete(&mut self, id: NodeId) -> bool {
        let lamp = self.lamport.tick();
        let actor_str = self.actor.to_string_hyphenated();
        let mut txn = self.doc.transact_mut();
        let Some(entry) = get_entry_map(&self.nodes, &txn, id) else {
            return false;
        };
        entry.insert(&mut txn, "deleted", true);
        entry.insert(&mut txn, "del_lamp", lamp.get() as i64);
        entry.insert(&mut txn, "del_actor", actor_str);
        true
    }

    /// Record that this actor modified the node's content. Used to
    /// beat a stale delete at projection time (§6.3). Does not touch
    /// `name` / `parent`.
    pub fn record_modify(&mut self, id: NodeId) -> bool {
        let lamp = self.lamport.tick();
        let actor_str = self.actor.to_string_hyphenated();
        let mut txn = self.doc.transact_mut();
        let Some(entry) = get_entry_map(&self.nodes, &txn, id) else {
            return false;
        };
        entry.insert(&mut txn, "mod_lamp", lamp.get() as i64);
        entry.insert(&mut txn, "mod_actor", actor_str);
        true
    }

    /// Update a binary's blob hash (after CAS push). Also stamps modify.
    pub fn set_blob_hash(&mut self, id: NodeId, hash: &str, size: u64) -> bool {
        let lamp = self.lamport.tick();
        let actor_str = self.actor.to_string_hyphenated();
        let mut txn = self.doc.transact_mut();
        let Some(entry) = get_entry_map(&self.nodes, &txn, id) else {
            return false;
        };
        entry.insert(&mut txn, "blob", hash.to_string());
        entry.insert(&mut txn, "size", size as i64);
        entry.insert(&mut txn, "mod_lamp", lamp.get() as i64);
        entry.insert(&mut txn, "mod_actor", actor_str);
        true
    }

    // ------------------------------------------------------------------
    // Read API
    // ------------------------------------------------------------------

    pub fn get_entry(&self, id: NodeId) -> Option<NodeEntry> {
        let txn = self.doc.transact();
        let entry_map = match self.nodes.get(&txn, &id.to_string_hyphenated())? {
            Out::YMap(m) => m,
            _ => return None,
        };
        decode_entry(id, &entry_map, &txn)
    }

    pub fn all_entries(&self) -> HashMap<NodeId, NodeEntry> {
        let txn = self.doc.transact();
        let mut out = HashMap::new();
        for (k, v) in self.nodes.iter(&txn) {
            let Some(id) = NodeId::parse_str(&k) else {
                continue;
            };
            let Out::YMap(m) = v else { continue };
            if let Some(entry) = decode_entry(id, &m, &txn) {
                out.insert(id, entry);
            }
        }
        out
    }

    pub fn live_entries(&self) -> Vec<NodeEntry> {
        self.all_entries()
            .into_values()
            .filter(|e| !e.deleted)
            .collect()
    }

    /// Look up an entry by its disk-relative path, **including
    /// tombstoned entries**.
    ///
    /// This differs from `Projection::by_path` (which filters
    /// tombstones away under §6.1 LWW) in that it walks the raw
    /// `(name, parent)` chain on every entry and yields a match
    /// regardless of `deleted` state.
    ///
    /// Used by `scan_once` to detect that an on-disk file
    /// corresponds to a tombstoned NodeId — and therefore must be
    /// removed locally rather than promoted into a fresh sibling
    /// NodeId via `create_text` / `create_binary`. Without this
    /// lookup, a peer that reconnects with stale files from before a
    /// delete propagated would resurrect every one of them on the
    /// entire network. See `DESIGN_DOC_V1.md` §5.2.
    ///
    /// On collision (a live and a tombstoned entry both project to
    /// the same raw path — possible during a same-path simultaneous
    /// create) the **live** entry is returned. Directory entries are
    /// not considered (directories never appear in
    /// `Projection::by_path`).
    pub fn find_entry_by_path(&self, path: &str) -> Option<NodeEntry> {
        let all = self.all_entries();
        let mut best: Option<NodeEntry> = None;
        for entry in all.values() {
            if entry.kind == NodeKind::Directory {
                continue;
            }
            let Some(p) = build_path_ignoring_tombstones(entry, &all) else {
                continue;
            };
            if p != path {
                continue;
            }
            match &best {
                Some(b) if !b.deleted && entry.deleted => {}
                _ => {
                    best = Some(entry.clone());
                }
            }
        }
        best
    }
}

/// Build a disk-relative path for `entry` by walking the parent chain,
/// **without** the projection's tombstoned-directory bail-out. The
/// scanner needs to recognise a tombstoned leaf even when its
/// directory chain itself is partially tombstoned but still on disk.
/// Returns `None` if the chain is broken (parent NodeId not present)
/// or pathologically deep.
fn build_path_ignoring_tombstones(
    entry: &NodeEntry,
    all: &HashMap<NodeId, NodeEntry>,
) -> Option<String> {
    let mut segments: Vec<String> = vec![entry.name.clone()];
    let mut cursor = entry.parent;
    let mut hops = 0usize;
    const MAX_HOPS: usize = 1024;
    while let Some(pid) = cursor {
        hops += 1;
        if hops > MAX_HOPS {
            return None;
        }
        let parent = all.get(&pid)?;
        segments.push(parent.name.clone());
        cursor = parent.parent;
    }
    segments.reverse();
    Some(segments.join("/"))
}

fn get_entry_map<T: ReadTxn>(nodes: &MapRef, txn: &T, id: NodeId) -> Option<MapRef> {
    match nodes.get(txn, &id.to_string_hyphenated())? {
        Out::YMap(m) => Some(m),
        _ => None,
    }
}

fn decode_entry<T: ReadTxn>(id: NodeId, m: &MapRef, txn: &T) -> Option<NodeEntry> {
    let name = match m.get(txn, "name") {
        Some(Out::Any(Any::String(s))) => s.to_string(),
        _ => return None,
    };
    let parent_str = match m.get(txn, "parent") {
        Some(Out::Any(Any::String(s))) => s.to_string(),
        _ => String::new(),
    };
    let parent = if parent_str.is_empty() {
        None
    } else {
        NodeId::parse_str(&parent_str)
    };
    let deleted = matches!(m.get(txn, "deleted"), Some(Out::Any(Any::Bool(true))));
    let kind = match m.get(txn, "kind") {
        Some(Out::Any(Any::String(s))) => NodeKind::from_str(&s)?,
        _ => return None,
    };
    let blob_str = match m.get(txn, "blob") {
        Some(Out::Any(Any::String(s))) => s.to_string(),
        _ => String::new(),
    };
    let blob_hash = if blob_str.is_empty() { None } else { Some(blob_str) };
    let size = read_u64(m, txn, "size").unwrap_or(0);
    let created_at = read_u64(m, txn, "created_at")
        .map(Lamport)
        .unwrap_or(Lamport::ZERO);
    let created_by = match m.get(txn, "c_actor") {
        Some(Out::Any(Any::String(s))) => ActorId::parse_str(&s)?,
        _ => return None,
    };
    let delete_stamp = read_stamp(m, txn, "del_lamp", "del_actor");
    let modify_stamp = read_stamp(m, txn, "mod_lamp", "mod_actor");

    Some(NodeEntry {
        id,
        name,
        parent,
        deleted,
        kind,
        blob_hash,
        size,
        created_at,
        created_by,
        delete_stamp,
        modify_stamp,
    })
}

fn read_stamp<T: ReadTxn>(
    m: &MapRef,
    txn: &T,
    lamp_key: &str,
    actor_key: &str,
) -> Option<Stamp> {
    let lamp = read_u64(m, txn, lamp_key)?;
    let actor = match m.get(txn, actor_key)? {
        Out::Any(Any::String(s)) => ActorId::parse_str(&s)?,
        _ => return None,
    };
    Some(Stamp::new(Lamport(lamp), actor))
}

fn read_u64<T: ReadTxn>(m: &MapRef, txn: &T, key: &str) -> Option<u64> {
    match m.get(txn, key)? {
        Out::Any(Any::BigInt(v)) => Some(v as u64),
        Out::Any(Any::Number(v)) => Some(v as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manifest_has_no_entries() {
        let m = Manifest::new(ActorId::new());
        assert!(m.all_entries().is_empty());
        assert!(m.live_entries().is_empty());
    }

    #[test]
    fn create_node_returns_retrievable_entry() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("note.md", None, NodeKind::Text, None, 42);
        let e = m.get_entry(id).expect("entry must exist");
        assert_eq!(e.id, id);
        assert_eq!(e.name, "note.md");
        assert_eq!(e.parent, None);
        assert!(!e.deleted);
        assert_eq!(e.kind, NodeKind::Text);
        assert_eq!(e.blob_hash, None);
        assert_eq!(e.size, 42);
    }

    #[test]
    fn create_node_bumps_lamport() {
        let mut m = Manifest::new(ActorId::new());
        assert_eq!(m.lamport(), Lamport::ZERO);
        m.create_node("a.md", None, NodeKind::Text, None, 0);
        assert_eq!(m.lamport(), Lamport(1));
        m.create_node("b.md", None, NodeKind::Text, None, 0);
        assert_eq!(m.lamport(), Lamport(2));
    }

    #[test]
    fn rename_updates_name_preserves_id() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("old.md", None, NodeKind::Text, None, 0);
        assert!(m.set_name(id, "new.md"));
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.name, "new.md");
        assert_eq!(e.id, id);
        assert!(e.modify_stamp.is_some());
    }

    #[test]
    fn set_parent_updates_parent() {
        let mut m = Manifest::new(ActorId::new());
        let dir = m.create_node("folder", None, NodeKind::Directory, None, 0);
        let file = m.create_node("file.md", None, NodeKind::Text, None, 0);
        assert!(m.set_parent(file, Some(dir)));
        let e = m.get_entry(file).unwrap();
        assert_eq!(e.parent, Some(dir));
    }

    #[test]
    fn delete_sets_flag_and_stamp() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("doomed.md", None, NodeKind::Text, None, 0);
        assert!(m.delete(id));
        let e = m.get_entry(id).unwrap();
        assert!(e.deleted);
        assert!(e.delete_stamp.is_some());
        // live_entries filters it out
        assert!(m.live_entries().iter().all(|e| e.id != id));
    }

    #[test]
    fn record_modify_sets_mod_stamp() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("f.md", None, NodeKind::Text, None, 0);
        let before = m.get_entry(id).unwrap().modify_stamp;
        m.record_modify(id);
        let after = m.get_entry(id).unwrap().modify_stamp;
        assert!(after > before);
    }

    #[test]
    fn update_on_missing_node_is_noop() {
        let mut m = Manifest::new(ActorId::new());
        let ghost = NodeId::new();
        assert!(!m.set_name(ghost, "nope"));
        assert!(!m.delete(ghost));
        assert!(!m.set_parent(ghost, None));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut m1 = Manifest::new(ActorId::new());
        let id = m1.create_node("r.md", None, NodeKind::Text, None, 7);
        let state = m1.encode_state_as_update();

        let m2 = Manifest::from_update(ActorId::new(), Lamport::ZERO, &state).unwrap();
        let e = m2.get_entry(id).unwrap();
        assert_eq!(e.name, "r.md");
        assert_eq!(e.size, 7);
    }

    #[test]
    fn apply_update_advances_lamport() {
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("a", None, NodeKind::Text, None, 0);
        m1.create_node("b", None, NodeKind::Text, None, 0);
        m1.create_node("c", None, NodeKind::Text, None, 0);
        assert_eq!(m1.lamport(), Lamport(3));

        let mut m2 = Manifest::new(ActorId::new());
        assert_eq!(m2.lamport(), Lamport::ZERO);
        m2.apply_update(&m1.encode_state_as_update()).unwrap();
        // Must be at least 3 after observing m1's stamps (lamport advance rule).
        assert!(m2.lamport().get() >= 3);
    }

    #[test]
    fn binary_node_preserves_blob_hash() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node(
            "img.png",
            None,
            NodeKind::Binary,
            Some("abcd1234"),
            1024,
        );
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.kind, NodeKind::Binary);
        assert_eq!(e.blob_hash.as_deref(), Some("abcd1234"));
        m.set_blob_hash(id, "ef567890", 2048);
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.blob_hash.as_deref(), Some("ef567890"));
        assert_eq!(e.size, 2048);
    }

    #[test]
    fn find_entry_by_path_returns_tombstoned() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("ghost.md", None, NodeKind::Text, None, 0);
        m.delete(id);
        let found = m.find_entry_by_path("ghost.md").expect("tombstone reachable");
        assert_eq!(found.id, id);
        assert!(found.deleted);
    }

    #[test]
    fn find_entry_by_path_returns_live() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("alive.md", None, NodeKind::Text, None, 0);
        let found = m.find_entry_by_path("alive.md").unwrap();
        assert_eq!(found.id, id);
        assert!(!found.deleted);
    }

    #[test]
    fn find_entry_by_path_returns_none_when_absent() {
        let m = Manifest::new(ActorId::new());
        assert!(m.find_entry_by_path("nope.md").is_none());
    }

    #[test]
    fn find_entry_by_path_prefers_live_over_tombstoned() {
        let mut m = Manifest::new(ActorId::new());
        let dead = m.create_node("collide.md", None, NodeKind::Text, None, 0);
        m.delete(dead);
        let live = m.create_node("collide.md", None, NodeKind::Text, None, 0);
        let found = m.find_entry_by_path("collide.md").unwrap();
        assert_eq!(found.id, live);
        assert!(!found.deleted);
    }

    #[test]
    fn find_entry_by_path_walks_directory_chain() {
        let mut m = Manifest::new(ActorId::new());
        let dir = m.create_node("folder", None, NodeKind::Directory, None, 0);
        let file = m.create_node("note.md", Some(dir), NodeKind::Text, None, 0);
        m.delete(file);
        let found = m.find_entry_by_path("folder/note.md").unwrap();
        assert_eq!(found.id, file);
        assert!(found.deleted);
    }

    #[test]
    fn find_entry_by_path_skips_directory_kind() {
        // A directory's "path" must not collide with a file lookup —
        // directories are emergent (§5.6), never projected as files.
        let mut m = Manifest::new(ActorId::new());
        let _dir = m.create_node("folder", None, NodeKind::Directory, None, 0);
        assert!(m.find_entry_by_path("folder").is_none());
    }

    #[test]
    fn two_clients_converge_on_same_state() {
        let mut m1 = Manifest::new(ActorId::new());
        let mut m2 = Manifest::new(ActorId::new());

        let id1 = m1.create_node("one.md", None, NodeKind::Text, None, 10);
        let id2 = m2.create_node("two.md", None, NodeKind::Text, None, 20);

        // Exchange updates
        let u1 = m1.encode_state_as_update();
        let u2 = m2.encode_state_as_update();
        m2.apply_update(&u1).unwrap();
        m1.apply_update(&u2).unwrap();

        // Both see both entries.
        for m in [&m1, &m2] {
            assert!(m.get_entry(id1).is_some());
            assert!(m.get_entry(id2).is_some());
        }
    }
}
