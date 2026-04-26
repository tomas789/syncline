//! One-way migration from v0's on-disk layout to a v1 [`Manifest`].
//!
//! Reads the per-file Y.Doc snapshots v0 stores under
//! `.syncline/data/<uuid>.bin`. Each snapshot has a `meta` Y.Map with
//! `path` / `type` / `blob_hash` fields and, for text files, a
//! `content` Y.Text. This module drains that into a fresh v1 manifest
//! plus an in-memory `text_contents` map that the caller uses to seed
//! the v1 content subdocs.
//!
//! Design-doc reference: §7. Migration is local to each client, one-
//! way, and non-destructive: the caller is expected to rename the v0
//! `.syncline/` directory to `.syncline.v0.bak/` before running this.

use super::ids::{ActorId, NodeId};
use super::manifest::{Manifest, NodeKind};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use yrs::{Any, Doc, GetString, Map, MapRef, Out, Transact};

/// Result of migrating one v0 vault.
pub struct Migration {
    /// The fresh v1 manifest, populated with one node per live v0 file
    /// plus directory nodes for every path prefix that has children.
    pub manifest: Manifest,
    /// For each text-kind NodeId, the body extracted from v0's `content`
    /// Y.Text. The caller populates content subdocs from these.
    pub text_contents: HashMap<NodeId, String>,
    /// For each binary-kind NodeId, the SHA-256 hex hash from v0's
    /// `meta.blob_hash`. The caller uploads blobs as needed.
    pub binary_hashes: HashMap<NodeId, String>,
    /// Per-file problems that didn't abort the migration but are worth
    /// surfacing in logs.
    pub warnings: Vec<String>,
}

/// Walk a v0 vault's `.syncline/data/*.bin` snapshots and build a v1
/// [`Manifest`]. Does not touch the filesystem beyond reading.
pub fn migrate_v0_vault(vault_root: &Path, actor: ActorId) -> Result<Migration> {
    let data_dir = vault_root.join(".syncline").join("data");
    if !data_dir.exists() {
        // Fresh vault — nothing to migrate.
        return Ok(Migration {
            manifest: Manifest::new(actor),
            text_contents: HashMap::new(),
            binary_hashes: HashMap::new(),
            warnings: Vec::new(),
        });
    }

    let mut manifest = Manifest::new(actor);
    let mut text_contents = HashMap::new();
    let mut binary_hashes = HashMap::new();
    let mut warnings = Vec::new();
    // Cache of path-prefix → NodeId for directory nodes, so we reuse
    // the same directory NodeId across all files that live under it.
    let mut dir_nodes: HashMap<String, NodeId> = HashMap::new();

    let snapshots = collect_snapshot_paths(&data_dir)?;
    for snap in snapshots {
        match read_snapshot(&snap) {
            Ok(Some(v0)) => {
                // v0 had a bug where a deleted file kept its .bin around
                // with an empty meta.path. Skip those.
                if v0.rel_path.is_empty() {
                    warnings.push(format!(
                        "skipped snapshot {} with empty meta.path",
                        snap.display()
                    ));
                    continue;
                }
                // v0's "delete = empty text content" projection is also
                // ghosted: if a text file has empty content AND no
                // blob_hash (so isn't a binary), treat as deleted.
                // Discussion in §7.4.
                if matches!(v0.kind, NodeKind::Text)
                    && v0.content.as_deref().map(str::is_empty).unwrap_or(true)
                {
                    warnings.push(format!(
                        "skipped ghost-deleted v0 text file {:?}",
                        v0.rel_path
                    ));
                    continue;
                }

                let parent = ensure_parent_chain(
                    &mut manifest,
                    &mut dir_nodes,
                    &v0.rel_path,
                );

                let leaf_name = leaf_segment(&v0.rel_path).to_string();
                let size = match v0.kind {
                    NodeKind::Text => v0.content.as_ref().map(|s| s.len()).unwrap_or(0) as u64,
                    NodeKind::Binary => 0,
                    NodeKind::Directory => 0,
                };

                // v0 stored a single blob hash per file. Project it as
                // a length-1 chunk list — the new schema's small-file
                // representation. Files large enough to need chunking
                // get re-chunked the next time the scanner sees them.
                let chunk_hashes: Vec<String> = v0
                    .blob_hash
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|h| vec![h.to_string()])
                    .unwrap_or_default();
                let node_id = manifest.create_node(
                    &leaf_name,
                    parent,
                    v0.kind,
                    &chunk_hashes,
                    size,
                );

                match v0.kind {
                    NodeKind::Text => {
                        if let Some(body) = v0.content {
                            text_contents.insert(node_id, body);
                        }
                    }
                    NodeKind::Binary => {
                        if let Some(h) = v0.blob_hash {
                            binary_hashes.insert(node_id, h);
                        }
                    }
                    NodeKind::Directory => {
                        // v0 had no directory docs — unreachable here.
                    }
                }
            }
            Ok(None) => {
                warnings.push(format!(
                    "skipped snapshot {} — missing meta fields",
                    snap.display()
                ));
            }
            Err(e) => {
                warnings.push(format!(
                    "snapshot {} failed to decode: {}",
                    snap.display(),
                    e
                ));
            }
        }
    }

    Ok(Migration {
        manifest,
        text_contents,
        binary_hashes,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn collect_snapshot_paths(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(data_dir).context("reading .syncline/data")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("bin") {
            out.push(path);
        }
    }
    // Deterministic order — stable NodeId assignment when the caller
    // cares about reproducibility in tests.
    out.sort();
    Ok(out)
}

struct V0Snapshot {
    rel_path: String,
    kind: NodeKind,
    blob_hash: Option<String>,
    content: Option<String>,
}

fn read_snapshot(path: &Path) -> Result<Option<V0Snapshot>> {
    use yrs::updates::decoder::Decode;
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let update = yrs::Update::decode_v1(&bytes)
        .with_context(|| format!("decoding v1 update in {}", path.display()))?;
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(update);
    }

    let meta: MapRef = doc.get_or_insert_map("meta");
    let rel_path = match read_meta_string(&doc, &meta, "path") {
        Some(s) => s,
        None => return Ok(None),
    };
    let kind_str = read_meta_string(&doc, &meta, "type").unwrap_or_else(|| "text".to_string());
    let kind = match kind_str.as_str() {
        "binary" => NodeKind::Binary,
        _ => NodeKind::Text,
    };
    let blob_hash = read_meta_string(&doc, &meta, "blob_hash")
        .filter(|s| !s.is_empty());

    let content = if matches!(kind, NodeKind::Text) {
        let t = doc.get_or_insert_text("content");
        let txn = doc.transact();
        Some(t.get_string(&txn))
    } else {
        None
    };

    Ok(Some(V0Snapshot {
        rel_path,
        kind,
        blob_hash,
        content,
    }))
}

fn read_meta_string(doc: &Doc, meta: &MapRef, key: &str) -> Option<String> {
    let txn = doc.transact();
    match meta.get(&txn, key) {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

/// Returns the leaf segment of a relative path (`"a/b/c.md"` → `"c.md"`).
fn leaf_segment(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Ensure every ancestor directory of `rel_path` exists as a Directory
/// node in the manifest. Returns the parent NodeId of the leaf (None
/// if the leaf sits at the vault root).
fn ensure_parent_chain(
    manifest: &mut Manifest,
    dir_nodes: &mut HashMap<String, NodeId>,
    rel_path: &str,
) -> Option<NodeId> {
    let segments: Vec<&str> = rel_path.split('/').collect();
    if segments.len() <= 1 {
        return None;
    }
    let prefix_segments = &segments[..segments.len() - 1];

    let mut parent: Option<NodeId> = None;
    let mut accum = String::new();
    for seg in prefix_segments {
        if !accum.is_empty() {
            accum.push('/');
        }
        accum.push_str(seg);

        let id = if let Some(id) = dir_nodes.get(&accum) {
            *id
        } else {
            let id = manifest.create_node(seg, parent, NodeKind::Directory, &[], 0);
            dir_nodes.insert(accum.clone(), id);
            id
        };
        parent = Some(id);
    }
    parent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::storage::save_doc;
    use crate::v1::projection::project;
    use std::path::Path;
    use tempfile::TempDir;
    use uuid::Uuid;
    use yrs::{Doc, Map, Text, Transact};

    /// Build a minimal v0 layout: `.syncline/data/<uuid>.bin` per file.
    fn make_v0_snapshot(
        data_dir: &Path,
        rel_path: &str,
        kind: &str,
        content: Option<&str>,
        blob_hash: Option<&str>,
    ) {
        let doc = Doc::new();
        let meta = doc.get_or_insert_map("meta");
        {
            let mut txn = doc.transact_mut();
            meta.insert(&mut txn, "path", rel_path.to_string());
            meta.insert(&mut txn, "type", kind.to_string());
            if let Some(h) = blob_hash {
                meta.insert(&mut txn, "blob_hash", h.to_string());
            }
        }
        if let Some(body) = content {
            let t = doc.get_or_insert_text("content");
            let mut txn = doc.transact_mut();
            t.insert(&mut txn, 0, body);
        }
        fs::create_dir_all(data_dir).unwrap();
        let path = data_dir.join(format!("{}.bin", Uuid::new_v4()));
        save_doc(&doc, &path).unwrap();
    }

    #[test]
    fn migrate_fresh_vault_is_empty() {
        let vault = TempDir::new().unwrap();
        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        assert!(m.manifest.live_entries().is_empty());
        assert!(m.text_contents.is_empty());
        assert!(m.binary_hashes.is_empty());
    }

    #[test]
    fn migrate_single_text_file() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "hello.md", "text", Some("world"), None);

        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        assert_eq!(m.manifest.live_entries().len(), 1);
        let entry = &m.manifest.live_entries()[0];
        assert_eq!(entry.name, "hello.md");
        assert_eq!(entry.parent, None);
        assert_eq!(entry.kind, NodeKind::Text);
        let body = m.text_contents.get(&entry.id).expect("body migrated");
        assert_eq!(body, "world");
    }

    #[test]
    fn migrate_nested_path_creates_directory_nodes() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "a/b/c.md", "text", Some("deep"), None);

        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        let entries = m.manifest.live_entries();
        // c.md + a + b = 3 nodes
        assert_eq!(entries.len(), 3);
        // Projection must reassemble the full path.
        let p = project(&m.manifest);
        let file_row = p
            .by_path
            .get("a/b/c.md")
            .expect("projection should rebuild nested path");
        assert_eq!(file_row.kind, NodeKind::Text);
    }

    #[test]
    fn migrate_shared_directory_reuses_node() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "folder/one.md", "text", Some("1"), None);
        make_v0_snapshot(&data, "folder/two.md", "text", Some("2"), None);

        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        let dir_count = m
            .manifest
            .live_entries()
            .iter()
            .filter(|e| e.kind == NodeKind::Directory && e.name == "folder")
            .count();
        assert_eq!(dir_count, 1, "shared folder must reuse the same dir node");

        let p = project(&m.manifest);
        assert!(p.by_path.contains_key("folder/one.md"));
        assert!(p.by_path.contains_key("folder/two.md"));
    }

    #[test]
    fn migrate_binary_file_preserves_hash() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "img.png", "binary", None, Some("cafebabe"));

        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        assert_eq!(m.manifest.live_entries().len(), 1);
        let entry = &m.manifest.live_entries()[0];
        assert_eq!(entry.kind, NodeKind::Binary);
        assert_eq!(entry.chunk_hashes, vec!["cafebabe".to_string()]);
        assert_eq!(entry.single_blob_hash(), Some("cafebabe"));
        assert_eq!(
            m.binary_hashes.get(&entry.id).map(|s| s.as_str()),
            Some("cafebabe")
        );
    }

    #[test]
    fn migrate_skips_ghost_deleted_empty_text() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        // v0 bug: delete stored an empty text doc with a valid meta.path.
        make_v0_snapshot(&data, "was_deleted.md", "text", Some(""), None);
        make_v0_snapshot(&data, "alive.md", "text", Some("still here"), None);

        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        let names: Vec<_> = m
            .manifest
            .live_entries()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["alive.md".to_string()]);
        assert!(m.warnings.iter().any(|w| w.contains("ghost-deleted")));
    }

    #[test]
    fn migrate_skips_snapshot_with_empty_path() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "", "text", Some("no path"), None);

        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        assert!(m.manifest.live_entries().is_empty());
        assert!(m.warnings.iter().any(|w| w.contains("empty meta.path")));
    }

    #[test]
    fn migrate_mixed_vault_projects_correctly() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "note.md", "text", Some("root note"), None);
        make_v0_snapshot(&data, "folder/sub.md", "text", Some("sub note"), None);
        make_v0_snapshot(&data, "folder/pic.png", "binary", None, Some("deadbeef"));

        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();
        let p = project(&m.manifest);

        assert_eq!(p.len(), 3, "three user files projected");
        assert!(p.by_path.contains_key("note.md"));
        assert!(p.by_path.contains_key("folder/sub.md"));
        assert!(p.by_path.contains_key("folder/pic.png"));
        assert_eq!(
            p.by_path["folder/pic.png"].chunk_hashes,
            vec!["deadbeef".to_string()],
        );
    }

    #[test]
    fn migrate_stable_regardless_of_scan_order() {
        // Two snapshots with paths chosen so their temp filenames could
        // be processed in either order; migration must still reuse the
        // shared `folder` dir node.
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        for i in 0..10 {
            make_v0_snapshot(
                &data,
                &format!("folder/file_{i:02}.md"),
                "text",
                Some(&format!("body {i}")),
                None,
            );
        }
        let m = migrate_v0_vault(vault.path(), ActorId::new()).unwrap();

        let dir_count = m
            .manifest
            .live_entries()
            .iter()
            .filter(|e| e.kind == NodeKind::Directory)
            .count();
        assert_eq!(dir_count, 1);

        let p = project(&m.manifest);
        assert_eq!(p.len(), 10);
    }
}
