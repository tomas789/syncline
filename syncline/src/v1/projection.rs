//! Project a [`Manifest`] into the vault namespace. This is the
//! read-only layer that takes the raw CRDT state, applies the
//! conflict-resolution rules from §6 of the design doc, and returns
//! "what files should exist on disk, and what are their paths".
//!
//! The projection is pure and deterministic given identical input —
//! two peers running with the same manifest will produce identical
//! path strings, including conflict suffixes.

use super::ids::{NodeId, Stamp};
use super::manifest::{Manifest, NodeEntry, NodeKind};
use std::collections::HashMap;

/// One row of the projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedEntry {
    pub id: NodeId,
    pub path: String,
    pub kind: NodeKind,
    /// FastCDC chunk-hash list of this binary's content, in file order.
    /// See [`super::NodeEntry::chunk_hashes`] for the size buckets.
    pub chunk_hashes: Vec<String>,
    pub size: u64,
    /// `true` if this row was moved into the conflict name-space
    /// because another entry already owns the winning path.
    pub is_conflict_copy: bool,
}

impl ProjectedEntry {
    /// Convenience for callers that pre-date #59 and only handle
    /// single-chunk binaries: returns the lone hash if `chunk_hashes`
    /// has exactly one entry, else `None`.
    pub fn single_blob_hash(&self) -> Option<&str> {
        if self.chunk_hashes.len() == 1 {
            Some(self.chunk_hashes[0].as_str())
        } else {
            None
        }
    }
}

/// Full projection of a manifest. Keyed by the final (post-conflict-
/// suffix) path so callers can directly decide what to write to disk.
#[derive(Clone, Debug, Default)]
pub struct Projection {
    pub by_path: HashMap<String, ProjectedEntry>,
    pub by_id: HashMap<NodeId, ProjectedEntry>,
}

impl Projection {
    pub fn contains_path(&self, p: &str) -> bool {
        self.by_path.contains_key(p)
    }

    pub fn get_by_id(&self, id: NodeId) -> Option<&ProjectedEntry> {
        self.by_id.get(&id)
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

/// Project the manifest. Applies, in order:
/// 1. Modify-wins-over-delete (§6.3).
/// 2. Path assembly via parent chains.
/// 3. Same-path collision → deterministic conflict suffixes (§6.4).
pub fn project(manifest: &Manifest) -> Projection {
    let all = manifest.all_entries();

    // Step 1: decide which entries are live after §6.3.
    let live: Vec<&NodeEntry> = all
        .values()
        .filter(|e| is_live(e))
        .collect();

    // Step 2: assemble paths. Directories contribute to paths but are
    // not themselves projected as files (they are emergent — §5.6).
    let mut rows: Vec<Row> = Vec::new();
    for e in &live {
        if e.kind == NodeKind::Directory {
            continue; // directories don't get a row of their own
        }
        let Some(path) = build_path(e, &all) else {
            continue; // broken parent chain — drop
        };
        rows.push(Row {
            entry: (*e).clone(),
            base_path: path,
        });
    }

    // Step 3: collapse same-path collisions deterministically.
    // Group by base_path, sort by stamp ascending, winner = argmin, losers get suffixes.
    let mut groups: HashMap<String, Vec<Row>> = HashMap::new();
    for r in rows {
        groups.entry(r.base_path.clone()).or_default().push(r);
    }

    let mut out = Projection::default();
    for (base_path, mut group) in groups {
        // Sort ascending by stamp; winner = first.
        group.sort_by(|a, b| {
            a.entry
                .effective_stamp()
                .cmp(&b.entry.effective_stamp())
                .then_with(|| a.entry.id.cmp(&b.entry.id))
        });

        for (i, row) in group.into_iter().enumerate() {
            let is_conflict = i > 0;
            let final_path = if is_conflict {
                conflict_path(&base_path, row.entry.effective_stamp(), row.entry.id)
            } else {
                base_path.clone()
            };
            let projected = ProjectedEntry {
                id: row.entry.id,
                path: final_path.clone(),
                kind: row.entry.kind,
                chunk_hashes: row.entry.chunk_hashes.clone(),
                size: row.entry.size,
                is_conflict_copy: is_conflict,
            };
            out.by_id.insert(projected.id, projected.clone());
            out.by_path.insert(final_path, projected);
        }
    }

    out
}

struct Row {
    entry: NodeEntry,
    base_path: String,
}

fn is_live(entry: &NodeEntry) -> bool {
    if !entry.deleted {
        return true;
    }
    // §6.3: a modification with a stamp beating the delete resurrects.
    match (entry.delete_stamp, entry.modify_stamp) {
        (Some(d), Some(m)) => m.beats(&d),
        (Some(_), None) => false,
        // deleted=true without a delete_stamp shouldn't happen in practice,
        // but if it does, fall back to the raw flag.
        (None, _) => false,
    }
}

fn build_path(entry: &NodeEntry, all: &HashMap<NodeId, NodeEntry>) -> Option<String> {
    let mut segments: Vec<String> = vec![entry.name.clone()];
    let mut cursor = entry.parent;
    // Guard against pathological cycles (shouldn't occur — see §6.6).
    let mut hops = 0;
    const MAX_HOPS: usize = 1024;
    while let Some(pid) = cursor {
        hops += 1;
        if hops > MAX_HOPS {
            return None;
        }
        let parent = all.get(&pid)?;
        if parent.deleted && parent.kind == NodeKind::Directory {
            // Parent directory is tombstoned → child is orphaned; drop.
            // (Cascade delete is expressed explicitly in §5.6 so this is
            // merely a safety net.)
            return None;
        }
        segments.push(parent.name.clone());
        cursor = parent.parent;
    }
    segments.reverse();
    Some(segments.join("/"))
}

/// Conflict-copy naming — deterministic across peers.
///
/// `foo.md` → `foo.conflict-<actor8>-<lamp>.md`
/// `bin`    → `bin.conflict-<actor8>-<lamp>`
fn conflict_path(base: &str, stamp: Stamp, id: NodeId) -> String {
    let (stem, ext) = split_ext(base);
    let actor_short = stamp.actor.short();
    let lamp = stamp.lamport.get();
    let node_short = &id.to_string_hyphenated()[..8];
    if let Some(ext) = ext {
        format!("{stem}.conflict-{actor_short}-{lamp}-{node_short}.{ext}")
    } else {
        format!("{stem}.conflict-{actor_short}-{lamp}-{node_short}")
    }
}

fn split_ext(path: &str) -> (&str, Option<&str>) {
    // Split on the last '.', but only in the final segment (don't
    // corrupt "foo.bar/baz").
    let last_slash = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    let last = &path[last_slash..];
    if let Some(dot) = last.rfind('.') {
        if dot == 0 {
            return (path, None); // ".hidden" → no extension
        }
        let stem_end = last_slash + dot;
        return (&path[..stem_end], Some(&path[stem_end + 1..]));
    }
    (path, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::ids::ActorId;

    #[test]
    fn empty_manifest_projects_empty() {
        let m = Manifest::new(ActorId::new());
        let p = project(&m);
        assert!(p.is_empty());
    }

    #[test]
    fn single_file_projects_to_its_name() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("hello.md", None, NodeKind::Text, &[], 5);
        let p = project(&m);
        assert_eq!(p.len(), 1);
        let row = p.get_by_id(id).unwrap();
        assert_eq!(row.path, "hello.md");
        assert!(!row.is_conflict_copy);
    }

    #[test]
    fn file_under_directory_has_joined_path() {
        let mut m = Manifest::new(ActorId::new());
        let dir = m.create_node("folder", None, NodeKind::Directory, &[], 0);
        let f = m.create_node("note.md", Some(dir), NodeKind::Text, &[], 0);
        let p = project(&m);
        // Directory itself is not projected as a file.
        assert_eq!(p.len(), 1);
        let row = p.get_by_id(f).unwrap();
        assert_eq!(row.path, "folder/note.md");
    }

    #[test]
    fn deep_nesting_assembles_full_path() {
        let mut m = Manifest::new(ActorId::new());
        let l1 = m.create_node("l1", None, NodeKind::Directory, &[], 0);
        let l2 = m.create_node("l2", Some(l1), NodeKind::Directory, &[], 0);
        let l3 = m.create_node("l3", Some(l2), NodeKind::Directory, &[], 0);
        let leaf = m.create_node("leaf.md", Some(l3), NodeKind::Text, &[], 0);
        let p = project(&m);
        assert_eq!(p.get_by_id(leaf).unwrap().path, "l1/l2/l3/leaf.md");
    }

    #[test]
    fn deleted_file_is_not_projected() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("gone.md", None, NodeKind::Text, &[], 0);
        m.delete(id);
        let p = project(&m);
        assert!(p.is_empty());
    }

    #[test]
    fn modify_after_delete_resurrects() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("phoenix.md", None, NodeKind::Text, &[], 0);
        m.delete(id); // delete with some lamport
        m.record_modify(id); // modify with a later lamport → resurrects
        let p = project(&m);
        assert_eq!(p.len(), 1);
        assert_eq!(p.get_by_id(id).unwrap().path, "phoenix.md");
    }

    #[test]
    fn delete_after_modify_stays_deleted() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("rip.md", None, NodeKind::Text, &[], 0);
        m.record_modify(id);
        m.delete(id);
        let p = project(&m);
        assert!(p.is_empty());
    }

    #[test]
    fn same_path_collision_gets_conflict_suffix() {
        // Two nodes created independently with the same (parent, name).
        // Rebuild by merging two manifests.
        let mut m1 = Manifest::new(ActorId::new());
        let id1 = m1.create_node("same.md", None, NodeKind::Text, &[], 0);

        let mut m2 = Manifest::new(ActorId::new());
        let id2 = m2.create_node("same.md", None, NodeKind::Text, &[], 0);

        // Merge
        let u1 = m1.encode_state_as_update();
        let u2 = m2.encode_state_as_update();
        m1.apply_update(&u2).unwrap();
        m2.apply_update(&u1).unwrap();

        // Both peers must produce the same projection deterministically.
        let p1 = project(&m1);
        let p2 = project(&m2);

        assert_eq!(p1.len(), 2, "both nodes live, one conflict");
        assert_eq!(p2.len(), 2);

        // Same set of paths on both sides.
        let mut paths1: Vec<_> = p1.by_path.keys().cloned().collect();
        let mut paths2: Vec<_> = p2.by_path.keys().cloned().collect();
        paths1.sort();
        paths2.sort();
        assert_eq!(paths1, paths2, "deterministic projection across peers");

        // One path is exactly "same.md", the other is a conflict copy.
        assert!(paths1.contains(&"same.md".to_string()));
        assert!(paths1.iter().any(|p| p.contains(".conflict-")));

        // Each node id exists exactly once across the two projections.
        let ids1: Vec<_> = p1.by_id.keys().cloned().collect();
        assert!(ids1.contains(&id1));
        assert!(ids1.contains(&id2));
    }

    #[test]
    fn conflict_suffix_preserves_extension() {
        let (stem, ext) = split_ext("foo/bar/baz.tar.gz");
        assert_eq!(stem, "foo/bar/baz.tar");
        assert_eq!(ext, Some("gz"));

        let (stem, ext) = split_ext("noext");
        assert_eq!(stem, "noext");
        assert_eq!(ext, None);

        let (stem, ext) = split_ext(".hidden");
        assert_eq!(stem, ".hidden");
        assert_eq!(ext, None);
    }

    #[test]
    fn binary_with_chunk_hashes_projected_correctly() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node(
            "image.png",
            None,
            NodeKind::Binary,
            &["cafebabe".to_string()],
            1024,
        );
        let p = project(&m);
        let row = p.get_by_id(id).unwrap();
        assert_eq!(row.kind, NodeKind::Binary);
        assert_eq!(row.chunk_hashes, vec!["cafebabe".to_string()]);
        assert_eq!(row.single_blob_hash(), Some("cafebabe"));
        assert_eq!(row.size, 1024);
        assert_eq!(row.path, "image.png");
    }

    #[test]
    fn rename_reflected_in_projection() {
        let mut m = Manifest::new(ActorId::new());
        let id = m.create_node("old.md", None, NodeKind::Text, &[], 0);
        m.set_name(id, "new.md");
        let p = project(&m);
        assert_eq!(p.get_by_id(id).unwrap().path, "new.md");
    }

    #[test]
    fn move_to_new_parent_reflected() {
        let mut m = Manifest::new(ActorId::new());
        let a = m.create_node("A", None, NodeKind::Directory, &[], 0);
        let b = m.create_node("B", None, NodeKind::Directory, &[], 0);
        let f = m.create_node("f.md", Some(a), NodeKind::Text, &[], 0);
        assert_eq!(project(&m).get_by_id(f).unwrap().path, "A/f.md");
        m.set_parent(f, Some(b));
        assert_eq!(project(&m).get_by_id(f).unwrap().path, "B/f.md");
    }

    #[test]
    fn orphan_with_deleted_parent_dir_is_hidden() {
        let mut m = Manifest::new(ActorId::new());
        let dir = m.create_node("doomed", None, NodeKind::Directory, &[], 0);
        let f = m.create_node("child.md", Some(dir), NodeKind::Text, &[], 0);
        m.delete(dir);
        let p = project(&m);
        // Child becomes unprojectable under the cascade-safety net.
        assert!(p.get_by_id(f).is_none());
    }
}
