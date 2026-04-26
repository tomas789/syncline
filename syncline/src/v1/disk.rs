//! On-disk v1 vault layout and the one-shot migration routine that
//! writes it from a v0 vault.
//!
//! v1 vault layout (inside the vault's hidden `.syncline/` dir):
//!
//! ```text
//! .syncline/
//!   version              — single line, "1"
//!   actor_id             — UUIDv4 for the local client (lamport tiebreak)
//!   manifest.bin         — encoded state update of the manifest Y.Doc
//!   content/<id>.bin     — per-text-file Y.Doc with a single "text" Y.Text
//!   blobs/<sha256>       — preserved from v0 (binary CAS)
//! ```
//!
//! `migrate_vault_on_disk` is **idempotent**: running it a second time
//! on a vault that already declares `version=1` is a no-op.
//!
//! The v0 layout is preserved on rename (`data` → `data.v0.bak`,
//! `path_map.json` → `path_map.json.v0.bak`) so a migration can be
//! rolled back manually if something looks off.

use super::ids::{ActorId, NodeId};
use super::migration::{migrate_v0_vault, Migration};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use yrs::{Doc, Text, Transact};

/// Summary of a migration run, intended for human-readable output.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub vault_root: PathBuf,
    pub actor_id: ActorId,
    pub text_files: usize,
    pub binary_files: usize,
    pub directories: usize,
    pub warnings: Vec<String>,
    /// `true` if the vault was already at v1 and nothing was written.
    pub already_migrated: bool,
}

/// Run the v0 → v1 migration against a vault directory in-place.
///
/// - If `.syncline/version` already reads `1`, returns early with
///   `already_migrated = true`.
/// - Otherwise builds a v1 manifest from the v0 snapshots, writes
///   `.syncline/manifest.bin`, `.syncline/content/<id>.bin` for every
///   text file, `.syncline/actor_id`, and `.syncline/version`.
/// - Renames v0 `data/` and `path_map.json` to `.v0.bak` siblings so
///   the old state is recoverable.
pub fn migrate_vault_on_disk(vault_root: &Path) -> Result<MigrationReport> {
    let syncline_dir = vault_root.join(".syncline");
    let version_path = syncline_dir.join("version");

    if let Ok(v) = fs::read_to_string(&version_path) {
        if v.trim() == "1" {
            return Ok(MigrationReport {
                vault_root: vault_root.to_path_buf(),
                actor_id: read_or_create_actor_id(&syncline_dir)?,
                text_files: 0,
                binary_files: 0,
                directories: 0,
                warnings: Vec::new(),
                already_migrated: true,
            });
        }
    }

    fs::create_dir_all(&syncline_dir).context("creating .syncline directory")?;
    let actor_id = read_or_create_actor_id(&syncline_dir)?;

    let Migration {
        manifest,
        text_contents,
        binary_hashes,
        warnings,
    } = migrate_v0_vault(vault_root, actor_id).context("scanning v0 vault")?;

    let text_files = text_contents.len();
    let binary_files = binary_hashes.len();
    let directories = manifest
        .live_entries()
        .iter()
        .filter(|e| e.kind == super::manifest::NodeKind::Directory)
        .count();

    // 1. Write per-text-file content subdocs.
    let content_dir = syncline_dir.join("content");
    fs::create_dir_all(&content_dir).context("creating .syncline/content")?;
    for (node_id, body) in &text_contents {
        write_content_subdoc(&content_dir, *node_id, body)
            .with_context(|| format!("writing content subdoc for node {node_id:?}"))?;
    }

    // 2. Write the manifest encoded state.
    let manifest_bytes = manifest.encode_state_as_update();
    atomic_write(&syncline_dir.join("manifest.bin"), &manifest_bytes)
        .context("writing manifest.bin")?;

    // 3. Preserve v0 data directory + path_map by renaming them.
    let data_dir = syncline_dir.join("data");
    if data_dir.exists() {
        let bak = syncline_dir.join("data.v0.bak");
        // If a previous aborted migration left a bak, keep the newer
        // one and timestamp the old one so nothing is lost.
        if bak.exists() {
            let alt = syncline_dir.join(format!(
                "data.v0.bak.{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ));
            fs::rename(&bak, &alt).context("relocating stale data.v0.bak")?;
        }
        fs::rename(&data_dir, &bak).context("renaming v0 data/ to data.v0.bak")?;
    }
    let pm = syncline_dir.join("path_map.json");
    if pm.exists() {
        let bak = syncline_dir.join("path_map.json.v0.bak");
        let _ = fs::remove_file(&bak); // previous bak is safe to discard
        fs::rename(&pm, &bak).context("renaming v0 path_map.json")?;
    }

    // 4. Last: version marker — future runs see v1 and no-op.
    atomic_write(&version_path, b"1\n").context("writing version marker")?;

    Ok(MigrationReport {
        vault_root: vault_root.to_path_buf(),
        actor_id,
        text_files,
        binary_files,
        directories,
        warnings,
        already_migrated: false,
    })
}

/// Read `.syncline/actor_id` if present, otherwise mint a fresh one
/// and persist it. Subsequent calls return the same actor for the
/// lifetime of the vault directory.
pub fn read_or_create_actor_id(syncline_dir: &Path) -> Result<ActorId> {
    let path = syncline_dir.join("actor_id");
    if let Ok(s) = fs::read_to_string(&path) {
        if let Some(id) = ActorId::parse_str(s.trim()) {
            return Ok(id);
        }
    }
    fs::create_dir_all(syncline_dir).context("creating .syncline for actor_id")?;
    let id = ActorId::new();
    atomic_write(&path, format!("{}\n", id.to_string_hyphenated()).as_bytes())
        .context("writing actor_id")?;
    Ok(id)
}

/// Read the `.syncline/version` marker. `None` if absent or unreadable.
pub fn read_vault_version(syncline_dir: &Path) -> Option<String> {
    fs::read_to_string(syncline_dir.join("version"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Build a fresh content Y.Doc with a single `text` Y.Text seeded from
/// `body`, then save to `<content_dir>/<node_id>.bin`.
fn write_content_subdoc(content_dir: &Path, node_id: NodeId, body: &str) -> Result<()> {
    let doc = Doc::new();
    let text = doc.get_or_insert_text("text");
    {
        let mut txn = doc.transact_mut();
        text.insert(&mut txn, 0, body);
    }
    let bytes = {
        use yrs::ReadTxn;
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    };
    let path = content_dir.join(format!("{}.bin", node_id.to_string_hyphenated()));
    atomic_write(&path, &bytes)?;
    Ok(())
}

/// Atomic write: temp file + rename. Skips fsync — see the longer
/// note in `client_v1.rs::atomic_write` for rationale (bulk bootstrap
/// fsync-per-file was the dominant cost; recovery is via re-sync).
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("creating parent directory")?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp).context("creating temp file")?;
        file.write_all(bytes).context("writing temp file")?;
    }
    fs::rename(&tmp, path).context("atomically renaming temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::storage::save_doc;
    use tempfile::TempDir;
    use uuid::Uuid;
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, GetString, Map, Transact};

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
        save_doc(&doc, &data_dir.join(format!("{}.bin", Uuid::new_v4()))).unwrap();
    }

    #[test]
    fn migrate_writes_expected_files() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "hello.md", "text", Some("world"), None);
        make_v0_snapshot(&data, "dir/pic.png", "binary", None, Some("deadbeef"));

        let report = migrate_vault_on_disk(vault.path()).unwrap();
        assert!(!report.already_migrated);
        assert_eq!(report.text_files, 1);
        assert_eq!(report.binary_files, 1);
        assert_eq!(report.directories, 1);

        let synced = vault.path().join(".syncline");
        assert!(synced.join("version").exists());
        assert_eq!(fs::read_to_string(synced.join("version")).unwrap().trim(), "1");
        assert!(synced.join("actor_id").exists());
        assert!(synced.join("manifest.bin").exists());
        assert!(synced.join("content").is_dir());
        assert!(synced.join("data.v0.bak").exists());
        assert!(!synced.join("data").exists());
    }

    #[test]
    fn migrate_content_subdoc_is_readable() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "body.md", "text", Some("hello world"), None);

        migrate_vault_on_disk(vault.path()).unwrap();

        let content_dir = vault.path().join(".syncline/content");
        let entries: Vec<_> = fs::read_dir(&content_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let path = entries[0].as_ref().unwrap().path();

        let bytes = fs::read(&path).unwrap();
        let update = yrs::Update::decode_v1(&bytes).unwrap();
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            txn.apply_update(update);
        }
        let text = doc.get_or_insert_text("text");
        let txn = doc.transact();
        assert_eq!(text.get_string(&txn), "hello world");
    }

    #[test]
    fn migrate_is_idempotent() {
        let vault = TempDir::new().unwrap();
        let data = vault.path().join(".syncline/data");
        make_v0_snapshot(&data, "one.md", "text", Some("first"), None);

        let first = migrate_vault_on_disk(vault.path()).unwrap();
        assert!(!first.already_migrated);
        let first_actor = first.actor_id;

        // Second run: should no-op and preserve the actor.
        let second = migrate_vault_on_disk(vault.path()).unwrap();
        assert!(second.already_migrated);
        assert_eq!(second.actor_id, first_actor);

        // Files are still in place.
        let synced = vault.path().join(".syncline");
        assert!(synced.join("manifest.bin").exists());
        assert!(synced.join("content").is_dir());
    }

    #[test]
    fn migrate_fresh_vault_produces_version_marker() {
        let vault = TempDir::new().unwrap();
        // No .syncline at all.
        let report = migrate_vault_on_disk(vault.path()).unwrap();
        assert!(!report.already_migrated);
        assert_eq!(report.text_files, 0);
        assert_eq!(report.binary_files, 0);
        assert!(vault.path().join(".syncline/version").exists());
        assert!(vault.path().join(".syncline/actor_id").exists());
    }

    #[test]
    fn actor_id_persists_across_calls() {
        let dir = TempDir::new().unwrap();
        let sd = dir.path().to_path_buf();
        let id1 = read_or_create_actor_id(&sd).unwrap();
        let id2 = read_or_create_actor_id(&sd).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn migrate_relocates_stale_bak_without_data_loss() {
        // Simulate a situation where a previous migration left a
        // data.v0.bak behind and THEN a v0 data/ directory reappeared
        // (e.g. user partially rolled back). Second migration must
        // preserve both directories.
        let vault = TempDir::new().unwrap();
        let synced = vault.path().join(".syncline");
        fs::create_dir_all(synced.join("data.v0.bak")).unwrap();
        fs::write(synced.join("data.v0.bak/leftover"), b"old").unwrap();
        let data = synced.join("data");
        make_v0_snapshot(&data, "fresh.md", "text", Some("new"), None);

        migrate_vault_on_disk(vault.path()).unwrap();

        // data.v0.bak now has the newly-moved v0 data; the earlier
        // backup was renamed to a timestamped sibling.
        let bak_contents: Vec<_> = fs::read_dir(&synced)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("data.v0.bak")
            })
            .collect();
        assert!(
            bak_contents.len() >= 2,
            "stale backup should have been preserved, found {bak_contents:?}"
        );
    }
}
