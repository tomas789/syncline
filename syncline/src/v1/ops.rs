//! v1 operation layer — path-level intents (create / delete / rename /
//! modify) expressed against a [`Manifest`].
//!
//! The manifest mutators in `manifest.rs` operate on `NodeId`s; this
//! module translates user-facing *paths* into those writes and owns
//! the emergent-directory rule (§5.6 of the design doc). Name /
//! parent changes go through `set_name` / `set_parent`; binary content
//! updates go through `set_chunk_hashes` and stamp modify. Conflict
//! handling (§6) remains projection-time.
//!
//! All operations are idempotent only in the CRDT sense: a second
//! call with the same intent may produce a second write and bump
//! lamports again. Callers that care should diff against the
//! projection first.

use super::ids::NodeId;
use super::manifest::{Manifest, NodeKind};
use super::projection::project;
use anyhow::{anyhow, Result};

/// Create a fresh text entry at `path`. Missing parent directories
/// are materialised as Directory nodes.
///
/// Errors:
/// - `path` is empty or has an empty segment.
/// - a live entry already projects to `path`.
pub fn create_text(manifest: &mut Manifest, path: &str, size: u64) -> Result<NodeId> {
    create_at_path(manifest, path, NodeKind::Text, &[], size)
}

/// Create a text entry at `path` even when another live entry already
/// projects there. Used by the scanner during bootstrap to record a
/// local file that shares a path with a remote manifest entry whose
/// content we have not yet observed: projection will resolve the
/// resulting collision deterministically via conflict suffixes.
pub fn create_text_allowing_collision(
    manifest: &mut Manifest,
    path: &str,
    size: u64,
) -> Result<NodeId> {
    create_at_path_allowing_collision(manifest, path, NodeKind::Text, &[], size)
}

/// Create a fresh binary entry at `path` with its FastCDC chunk-hash
/// list. Pass an empty slice to record a binary placeholder whose
/// content has not been chunked yet.
pub fn create_binary(
    manifest: &mut Manifest,
    path: &str,
    chunk_hashes: &[String],
    size: u64,
) -> Result<NodeId> {
    create_at_path(manifest, path, NodeKind::Binary, chunk_hashes, size)
}

/// Mark the entry currently projected at `path` as deleted.
pub fn delete(manifest: &mut Manifest, path: &str) -> Result<()> {
    let id = resolve_path(manifest, path)?;
    if !manifest.delete(id) {
        return Err(anyhow!("manifest delete failed for {:?}", id));
    }
    Ok(())
}

/// Rename or move the entry at `from` to `to`. A same-parent rename
/// updates only `name`; a cross-parent move updates `parent` too, and
/// creates the target's parent chain if needed.
///
/// Errors:
/// - `from` does not resolve.
/// - `to` is already occupied by a different live entry.
/// - `to` is malformed (empty leaf).
pub fn rename(manifest: &mut Manifest, from: &str, to: &str) -> Result<()> {
    if from == to {
        return Ok(());
    }
    let src_id = resolve_path(manifest, from)?;
    {
        let proj = project(manifest);
        if proj.by_path.contains_key(to) {
            return Err(anyhow!("target path {:?} already occupied", to));
        }
    }

    let (parent_path, leaf) = split_path(to);
    if leaf.is_empty() {
        return Err(anyhow!("rename target {:?} has empty leaf", to));
    }

    let current = manifest
        .get_entry(src_id)
        .ok_or_else(|| anyhow!("source {:?} vanished mid-rename", src_id))?;
    let new_parent = ensure_parent_chain(manifest, parent_path)?;

    if current.name != leaf {
        manifest.set_name(src_id, leaf);
    }
    if current.parent != new_parent {
        manifest.set_parent(src_id, new_parent);
    }
    Ok(())
}

/// Stamp a text-content modification on the entry at `path`. Used to
/// beat a stale delete under the modify-wins-over-delete rule (§6.3).
pub fn record_modify_text(manifest: &mut Manifest, path: &str) -> Result<()> {
    let id = resolve_path(manifest, path)?;
    manifest.record_modify(id);
    Ok(())
}

/// Update the chunk-hash list of a binary entry at `path`. Also stamps
/// modify so a stale delete can't resurrect older content.
pub fn record_modify_binary(
    manifest: &mut Manifest,
    path: &str,
    chunk_hashes: &[String],
    size: u64,
) -> Result<()> {
    let id = resolve_path(manifest, path)?;
    manifest.set_chunk_hashes(id, chunk_hashes, size);
    Ok(())
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn resolve_path(manifest: &Manifest, path: &str) -> Result<NodeId> {
    let proj = project(manifest);
    proj.by_path
        .get(path)
        .map(|r| r.id)
        .ok_or_else(|| anyhow!("no entry at path {:?}", path))
}

fn create_at_path(
    manifest: &mut Manifest,
    path: &str,
    kind: NodeKind,
    chunk_hashes: &[String],
    size: u64,
) -> Result<NodeId> {
    if path.is_empty() {
        return Err(anyhow!("empty path"));
    }
    {
        let proj = project(manifest);
        if proj.by_path.contains_key(path) {
            return Err(anyhow!("path {:?} already exists", path));
        }
    }
    let (parent_path, leaf) = split_path(path);
    if leaf.is_empty() {
        return Err(anyhow!("path {:?} has empty leaf", path));
    }
    let parent = ensure_parent_chain(manifest, parent_path)?;
    Ok(manifest.create_node(leaf, parent, kind, chunk_hashes, size))
}

fn create_at_path_allowing_collision(
    manifest: &mut Manifest,
    path: &str,
    kind: NodeKind,
    chunk_hashes: &[String],
    size: u64,
) -> Result<NodeId> {
    if path.is_empty() {
        return Err(anyhow!("empty path"));
    }
    let (parent_path, leaf) = split_path(path);
    if leaf.is_empty() {
        return Err(anyhow!("path {:?} has empty leaf", path));
    }
    let parent = ensure_parent_chain(manifest, parent_path)?;
    Ok(manifest.create_node(leaf, parent, kind, chunk_hashes, size))
}

fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Ensure every segment of `parent_path` is materialised as a live
/// directory node. Returns the leaf parent's NodeId, or `None` when
/// the target lives at the vault root.
fn ensure_parent_chain(manifest: &mut Manifest, parent_path: &str) -> Result<Option<NodeId>> {
    if parent_path.is_empty() {
        return Ok(None);
    }

    let mut parent: Option<NodeId> = None;
    for seg in parent_path.split('/') {
        if seg.is_empty() {
            return Err(anyhow!("empty segment in path {:?}", parent_path));
        }
        parent = Some(find_or_create_directory(manifest, parent, seg));
    }
    Ok(parent)
}

fn find_or_create_directory(
    manifest: &mut Manifest,
    parent: Option<NodeId>,
    name: &str,
) -> NodeId {
    let existing = manifest.all_entries().into_values().find(|e| {
        !e.deleted && e.kind == NodeKind::Directory && e.name == name && e.parent == parent
    });
    if let Some(e) = existing {
        e.id
    } else {
        manifest.create_node(name, parent, NodeKind::Directory, &[], 0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ids::ActorId;
    use super::super::sync::{handle_manifest_payload, manifest_step1_payload};
    use super::*;

    #[test]
    fn create_text_at_root() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_text(&mut m, "note.md", 5).unwrap();
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.name, "note.md");
        assert_eq!(e.parent, None);
        assert_eq!(e.size, 5);
        let p = project(&m);
        assert!(p.by_path.contains_key("note.md"));
    }

    #[test]
    fn create_text_at_nested_path_creates_dir_chain() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_text(&mut m, "a/b/c.md", 0).unwrap();
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.name, "c.md");
        let p = project(&m);
        assert!(p.by_path.contains_key("a/b/c.md"));
    }

    #[test]
    fn create_rejects_empty_path() {
        let mut m = Manifest::new(ActorId::new());
        assert!(create_text(&mut m, "", 0).is_err());
    }

    #[test]
    fn create_rejects_trailing_slash() {
        let mut m = Manifest::new(ActorId::new());
        assert!(create_text(&mut m, "dir/", 0).is_err());
    }

    #[test]
    fn create_rejects_duplicate_path() {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "f.md", 0).unwrap();
        assert!(create_text(&mut m, "f.md", 0).is_err());
    }

    #[test]
    fn create_binary_preserves_chunks() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_binary(
            &mut m,
            "img.png",
            &["deadbeef".to_string()],
            1024,
        )
        .unwrap();
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.kind, NodeKind::Binary);
        assert_eq!(e.chunk_hashes, vec!["deadbeef".to_string()]);
        assert_eq!(e.size, 1024);
    }

    #[test]
    fn create_binary_preserves_multi_chunk() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_binary(
            &mut m,
            "big.png",
            &["aaa".to_string(), "bbb".to_string(), "ccc".to_string()],
            6_000_000,
        )
        .unwrap();
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.chunk_hashes.len(), 3);
        assert_eq!(e.chunk_hashes[1], "bbb");
        assert_eq!(e.size, 6_000_000);
    }

    #[test]
    fn siblings_share_parent_directory() {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "folder/a.md", 0).unwrap();
        create_text(&mut m, "folder/b.md", 0).unwrap();
        let dir_count = m
            .live_entries()
            .iter()
            .filter(|e| e.kind == NodeKind::Directory && e.name == "folder")
            .count();
        assert_eq!(dir_count, 1);
    }

    #[test]
    fn delete_removes_from_projection() {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "doomed.md", 0).unwrap();
        delete(&mut m, "doomed.md").unwrap();
        let p = project(&m);
        assert!(!p.by_path.contains_key("doomed.md"));
    }

    #[test]
    fn delete_errors_when_missing() {
        let mut m = Manifest::new(ActorId::new());
        assert!(delete(&mut m, "ghost.md").is_err());
    }

    #[test]
    fn rename_same_parent_updates_name() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_text(&mut m, "old.md", 0).unwrap();
        rename(&mut m, "old.md", "new.md").unwrap();
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.name, "new.md");
        assert_eq!(e.parent, None);
        let p = project(&m);
        assert!(p.by_path.contains_key("new.md"));
        assert!(!p.by_path.contains_key("old.md"));
    }

    #[test]
    fn rename_cross_parent_updates_parent() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_text(&mut m, "src/file.md", 0).unwrap();
        rename(&mut m, "src/file.md", "dst/file.md").unwrap();
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.name, "file.md");
        let p = project(&m);
        assert!(p.by_path.contains_key("dst/file.md"));
        assert!(!p.by_path.contains_key("src/file.md"));
    }

    #[test]
    fn rename_creates_target_directory_chain() {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "note.md", 0).unwrap();
        rename(&mut m, "note.md", "a/b/note.md").unwrap();
        let p = project(&m);
        assert!(p.by_path.contains_key("a/b/note.md"));
    }

    #[test]
    fn rename_rejects_occupied_target() {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "a.md", 0).unwrap();
        create_text(&mut m, "b.md", 0).unwrap();
        assert!(rename(&mut m, "a.md", "b.md").is_err());
    }

    #[test]
    fn rename_rejects_missing_source() {
        let mut m = Manifest::new(ActorId::new());
        assert!(rename(&mut m, "ghost.md", "new.md").is_err());
    }

    #[test]
    fn rename_same_path_is_noop() {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "x.md", 0).unwrap();
        let before = m.lamport();
        rename(&mut m, "x.md", "x.md").unwrap();
        // No lamport tick — intentional no-op.
        assert_eq!(m.lamport(), before);
    }

    #[test]
    fn record_modify_text_bumps_stamp() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_text(&mut m, "m.md", 0).unwrap();
        let before = m.get_entry(id).unwrap().modify_stamp;
        record_modify_text(&mut m, "m.md").unwrap();
        let after = m.get_entry(id).unwrap().modify_stamp;
        assert!(after > before);
    }

    #[test]
    fn record_modify_binary_updates_chunks_and_size() {
        let mut m = Manifest::new(ActorId::new());
        let id = create_binary(&mut m, "pic.png", &["aaaa".to_string()], 10).unwrap();
        record_modify_binary(
            &mut m,
            "pic.png",
            &["bbbb".to_string(), "cccc".to_string()],
            42,
        )
        .unwrap();
        let e = m.get_entry(id).unwrap();
        assert_eq!(e.chunk_hashes, vec!["bbbb".to_string(), "cccc".to_string()]);
        assert_eq!(e.size, 42);
        assert!(e.modify_stamp.is_some());
    }

    #[test]
    fn modify_beats_earlier_delete() {
        // Scenario: A deletes, B observes the delete via sync, then B
        // modifies the (tombstoned) entry. B's modify stamp is
        // strictly newer than A's delete stamp, so projection must
        // resurrect the entry on both peers.
        let mut a = Manifest::new(ActorId::new());
        let id = create_text(&mut a, "race.md", 0).unwrap();
        let mut b =
            Manifest::from_update(ActorId::new(), a.lamport(), &a.encode_state_as_update())
                .unwrap();

        // A deletes first.
        delete(&mut a, "race.md").unwrap();

        // B observes A's delete.
        let sb = manifest_step1_payload(&b);
        let r_for_b = handle_manifest_payload(&mut a, &sb).unwrap().unwrap();
        handle_manifest_payload(&mut b, &r_for_b).unwrap();
        // Sanity: B now sees the tombstone.
        let be = b.get_entry(id).unwrap();
        assert!(be.deleted, "B should see A's delete before modifying");

        // B modifies the entry by the NodeId it was already holding
        // (the path no longer resolves because projection filtered the
        // tombstoned entry — in real use the editor kept the handle).
        // B's mod_lamp ends up strictly > A's del_lamp.
        assert!(b.record_modify(id));

        // Sync the modify back to A.
        let sa = manifest_step1_payload(&a);
        let r_for_a = handle_manifest_payload(&mut b, &sa).unwrap().unwrap();
        handle_manifest_payload(&mut a, &r_for_a).unwrap();

        // Both peers agree: modify won over delete.
        for (label, m) in [("a", &a), ("b", &b)] {
            let p = project(m);
            assert!(
                p.by_path.contains_key("race.md"),
                "peer {label}: modify-wins-over-delete violated (node {id:?})"
            );
        }
    }

    #[test]
    fn ops_converge_under_sync() {
        let mut a = Manifest::new(ActorId::new());
        let mut b = Manifest::new(ActorId::new());

        create_text(&mut a, "a1.md", 0).unwrap();
        create_text(&mut a, "shared/from_a.md", 0).unwrap();
        create_text(&mut b, "b1.md", 0).unwrap();
        create_text(&mut b, "shared/from_b.md", 0).unwrap();

        let sa = manifest_step1_payload(&a);
        let sb = manifest_step1_payload(&b);
        let r_for_a = handle_manifest_payload(&mut b, &sa).unwrap().unwrap();
        let r_for_b = handle_manifest_payload(&mut a, &sb).unwrap().unwrap();
        handle_manifest_payload(&mut a, &r_for_a).unwrap();
        handle_manifest_payload(&mut b, &r_for_b).unwrap();

        let pa = project(&a);
        let pb = project(&b);
        for p in ["a1.md", "b1.md", "shared/from_a.md", "shared/from_b.md"] {
            assert!(pa.by_path.contains_key(p), "A missing {p}");
            assert!(pb.by_path.contains_key(p), "B missing {p}");
        }
    }
}
