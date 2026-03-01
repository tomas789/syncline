use crate::client::diff::apply_diff_to_yrs;
use crate::client::storage::{load_doc, save_doc};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use yrs::{Any, Doc, GetString, Map, MapRef, Out, ReadTxn, Text, Transact};

// ---------------------------------------------------------------------------
// PathMap — persists the (relative_path → UUID) mapping across restarts.
// Stored as `.syncline/path_map.json`.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct PathMapData {
    entries: HashMap<String, String>, // relative_path → uuid
}

pub struct PathMap {
    data: PathMapData,
    file_path: PathBuf,
}

impl PathMap {
    fn load(syncline_parent: &Path) -> Self {
        let file_path = syncline_parent.join("path_map.json");
        let data = fs::read_to_string(&file_path)
            .ok()
            .and_then(|s| serde_json::from_str::<PathMapData>(&s).ok())
            .unwrap_or_default();
        Self { data, file_path }
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string(&self.data)?;
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.file_path, json)?;
        Ok(())
    }

    pub fn get_uuid(&self, rel_path: &str) -> Option<&str> {
        self.data.entries.get(rel_path).map(|s| s.as_str())
    }

    pub fn get_path_for_uuid(&self, uuid: &str) -> Option<&str> {
        self.data
            .entries
            .iter()
            .find(|(_, u)| u.as_str() == uuid)
            .map(|(p, _)| p.as_str())
    }

    pub fn insert(&mut self, rel_path: String, uuid: String) {
        self.data.entries.insert(rel_path, uuid);
    }

    pub fn remove_by_path(&mut self, rel_path: &str) -> Option<String> {
        self.data.entries.remove(rel_path)
    }

    pub fn remove_by_uuid(&mut self, uuid: &str) -> Option<String> {
        let key = self
            .data
            .entries
            .iter()
            .find(|(_, u)| u.as_str() == uuid)
            .map(|(p, _)| p.clone());
        key.and_then(|k| self.data.entries.remove(&k))
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.data
            .entries
            .iter()
            .map(|(p, u)| (p.as_str(), u.as_str()))
    }
}

// ---------------------------------------------------------------------------
// Helpers for reading/writing meta.path from a Yrs Y.Map
// ---------------------------------------------------------------------------

/// Read the "path" field from the "meta" Y.Map in a doc.
pub fn read_meta_path(doc: &Doc) -> Option<String> {
    let meta: MapRef = doc.get_or_insert_map("meta");
    let txn = doc.transact();
    match meta.get(&txn, "path") {
        Some(Out::Any(Any::String(arc))) => Some(arc.to_string()),
        _ => None,
    }
}

/// Write the "path" field to the "meta" Y.Map in a doc, returning the
/// encoded CRDT update (starting from the given state vector).
pub fn write_meta_path(doc: &Doc, rel_path: &str) {
    let meta: MapRef = doc.get_or_insert_map("meta");
    let mut txn = doc.transact_mut();
    meta.insert(&mut txn, "path", rel_path);
}

// ---------------------------------------------------------------------------
// LocalState
// ---------------------------------------------------------------------------

pub struct LocalState {
    pub root_dir: PathBuf,
    pub raw_root_dir: PathBuf,
    /// `.syncline/data/` — where `.bin` files and `path_map.json` live.
    pub syncline_dir: PathBuf,
    /// Stable unique identifier for this vault, used in conflict filenames.
    /// Persisted in `.syncline/client_id` so it survives restarts.
    pub client_name: String,
    /// Maps relative file paths to permanent UUIDs and back.
    pub path_map: PathMap,
}

impl LocalState {
    pub fn new(root_dir: impl AsRef<Path>, name_override: Option<String>) -> Self {
        let raw_root_dir = root_dir.as_ref().to_path_buf();
        // Canonicalize so that path comparisons with FSEvents-reported paths
        // work correctly on macOS, where /var is a symlink to /private/var and
        // FSEvents always returns the real (canonical) path.
        let root_dir = root_dir
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| root_dir.as_ref().to_path_buf());
        let syncline_dir = root_dir.join(".syncline").join("data");
        let syncline_parent = root_dir.join(".syncline");
        let client_id_path = syncline_parent.join("client_id");

        // Resolve the client name, persisting it for future runs:
        // 1. If --name was provided, use it and save it.
        // 2. If a saved client_id exists, use that.
        // 3. Otherwise, generate "hostname-<short_uuid>" and save it.
        let client_name = if let Some(name) = name_override {
            let _ = fs::create_dir_all(client_id_path.parent().unwrap_or(Path::new("")));
            let _ = fs::write(&client_id_path, &name);
            name
        } else if let Ok(stored) = fs::read_to_string(&client_id_path) {
            let trimmed = stored.trim().to_string();
            if trimmed.is_empty() {
                Self::generate_and_save_client_name(&client_id_path)
            } else {
                trimmed
            }
        } else {
            Self::generate_and_save_client_name(&client_id_path)
        };

        let path_map = PathMap::load(&syncline_parent);

        Self {
            root_dir,
            raw_root_dir,
            syncline_dir,
            client_name,
            path_map,
        }
    }

    fn generate_and_save_client_name(client_id_path: &Path) -> String {
        let hostname = gethostname::gethostname()
            .into_string()
            .unwrap_or_else(|_| "unknown".to_string());
        let short_id = &uuid::Uuid::new_v4().to_string()[..6];
        let generated = format!("{}-{}", hostname, short_id);
        let _ = fs::create_dir_all(client_id_path.parent().unwrap_or(Path::new("")));
        let _ = fs::write(client_id_path, &generated);
        generated
    }

    /// Converts a physical path to a relative doc path (e.g., "notes/idea.md").
    /// Used for path normalization — NOT for generating doc_ids directly.
    pub fn get_doc_id(&self, physical_path: &Path) -> Result<String> {
        // Canonicalize so that symlink-vs-real-path mismatches (e.g. macOS
        // /var → /private/var) don't cause strip_prefix to fail.
        let canonical = physical_path
            .canonicalize()
            .unwrap_or_else(|_| physical_path.to_path_buf());
        let rel = canonical.strip_prefix(&self.root_dir).or_else(|e| {
            // Fallback: try stripping the non-canonicalized root dir
            canonical.strip_prefix(&self.raw_root_dir).map_err(|_| e)
        })?;
        Ok(rel.to_string_lossy().to_string())
    }

    /// Get or create a UUID for the given relative path. The mapping is
    /// persisted in `.syncline/path_map.json`.
    pub fn get_or_create_uuid(&mut self, rel_path: &str) -> String {
        if let Some(uuid) = self.path_map.get_uuid(rel_path) {
            return uuid.to_string();
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        self.path_map.insert(rel_path.to_string(), uuid.clone());
        uuid
    }

    /// Returns the path to the binary Yrs snapshot for a given UUID.
    /// Stored as `.syncline/data/{uuid}.bin` (flat, not nested).
    pub fn get_state_path_for_uuid(&self, uuid: &str) -> PathBuf {
        self.syncline_dir.join(format!("{}.bin", uuid))
    }

    /// Scans the directory on startup. Compares physical files with `.syncline` state.
    /// Creates documents for missing states, applies diffs for modified files.
    ///
    /// Also performs content-based rename detection: if a file disappeared but a new
    /// file with identical CRDT content appeared, it is treated as a rename.
    ///
    /// Returns:
    /// - A list of `(uuid, update)` tuples that were modified offline and need to be synced.
    /// - A set of UUIDs that were **freshly created** (had no `.bin` state before this call).
    ///   This set is used by the sync client to detect same-path file conflicts.
    pub fn bootstrap_offline_changes(&mut self) -> Result<(Vec<(String, Vec<u8>)>, HashSet<String>)> {
        let mut offline_changes = Vec::new();
        let mut freshly_created: HashSet<String> = HashSet::new();

        // ---- Phase 1: Collect all disk files --------------------------------
        struct DiskFile {
            rel_path: String,
            disk_content: String,
        }
        let mut disk_files: Vec<DiskFile> = Vec::new();

        for entry in WalkDir::new(&self.root_dir)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                name != ".git" && name != ".syncline"
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "md" && ext != "txt" {
                continue;
            }

            let rel_path = match self.get_doc_id(path) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Failed to get rel_path for {:?}: {}", path, e);
                    continue;
                }
            };

            // Skip hidden path components
            if rel_path.split('/').any(|c| c.starts_with('.')) {
                continue;
            }

            let disk_content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            disk_files.push(DiskFile {
                rel_path,
                disk_content,
            });
        }

        // ---- Phase 2: Separate known vs new files ---------------------------
        // known: rel_path already in path_map
        // new: not in path_map → potential new file or rename target

        let mut known_files: Vec<(String, String, String)> = Vec::new(); // (rel_path, disk_content, uuid)
        let mut new_files: Vec<(String, String)> = Vec::new(); // (rel_path, disk_content)

        for df in disk_files {
            if let Some(uuid) = self.path_map.get_uuid(&df.rel_path) {
                known_files.push((df.rel_path, df.disk_content, uuid.to_string()));
            } else {
                new_files.push((df.rel_path, df.disk_content));
            }
        }

        // ---- Phase 3: Find orphaned UUIDs -----------------------------------
        // An orphaned UUID is one whose mapped path is not on disk (possibly renamed/deleted).

        let known_paths: HashSet<&str> = known_files.iter().map(|(rp, _, _)| rp.as_str()).collect();

        struct Orphaned {
            uuid: String,
            old_rel_path: String,
            crdt_content: String,
        }
        let mut orphaned: Vec<Orphaned> = Vec::new();

        let all_path_map_entries: Vec<(String, String)> = self
            .path_map
            .entries()
            .filter(|(p, _)| *p != "__index__")
            .map(|(p, u)| (p.to_string(), u.to_string()))
            .collect();

        for (rel_path, uuid) in &all_path_map_entries {
            if !known_paths.contains(rel_path.as_str()) {
                let state_path = self.get_state_path_for_uuid(uuid);
                if state_path.exists() {
                    let crdt_content = load_doc(&state_path)
                        .map(|doc| {
                            let text = doc.get_or_insert_text("content");
                            text.get_string(&doc.transact())
                        })
                        .unwrap_or_default();
                    orphaned.push(Orphaned {
                        uuid: uuid.clone(),
                        old_rel_path: rel_path.clone(),
                        crdt_content,
                    });
                }
            }
        }

        // ---- Phase 4: Content-based rename detection ------------------------
        // If a new file's disk content exactly matches an orphaned UUID's CRDT content,
        // treat it as a rename (preserve UUID and CRDT history).

        let mut renamed_map: HashMap<String, String> = HashMap::new(); // uuid → new_rel_path
        let mut unmatched_new: Vec<(String, String)> = Vec::new();

        for (new_rel_path, disk_content) in new_files {
            let matched = orphaned
                .iter()
                .position(|o| o.crdt_content == disk_content);

            if let Some(idx) = matched {
                let o = orphaned.remove(idx);
                renamed_map.insert(o.uuid.clone(), new_rel_path.clone());
                // Update path_map: remove old entry, add new one
                self.path_map.remove_by_path(&o.old_rel_path);
                self.path_map.insert(new_rel_path.clone(), o.uuid.clone());
                known_files.push((new_rel_path, disk_content, o.uuid));
            } else {
                unmatched_new.push((new_rel_path, disk_content));
            }
        }

        // ---- Phase 5: Process known files (including renamed ones) ----------

        for (rel_path, disk_content, uuid) in &known_files {
            let state_path = self.get_state_path_for_uuid(uuid);
            let was_renamed = renamed_map.contains_key(uuid.as_str());

            if state_path.exists() {
                // File existed before. Check for offline modification OR rename.
                let doc = match load_doc(&state_path) {
                    Ok(doc) => doc,
                    Err(_) => continue,
                };

                // Take previous_sv BEFORE any writes so the broadcast delta includes
                // meta.path changes (needed by other clients to locate the file on disk).
                let previous_sv = doc.transact().state_vector();

                // Ensure meta.path is up to date (important for renamed files).
                let current_meta = read_meta_path(&doc);
                let meta_path_changed = current_meta.as_deref() != Some(rel_path.as_str());
                if meta_path_changed {
                    write_meta_path(&doc, rel_path);
                }

                let text_ref = doc.get_or_insert_text("content");
                let yrs_content = text_ref.get_string(&doc.transact());

                if *disk_content != yrs_content || was_renamed || meta_path_changed {
                    if *disk_content != yrs_content {
                        apply_diff_to_yrs(&doc, &text_ref, &yrs_content, disk_content);
                    }
                    if let Err(e) = save_doc(&doc, &state_path) {
                        tracing::error!("Failed to save offline edits for {}: {}", uuid, e);
                        continue;
                    }
                    let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                    offline_changes.push((uuid.clone(), update));
                }
            } else {
                // First time this file is seen with its current UUID (shouldn't normally
                // happen since get_or_create_uuid also creates the .bin, but handle it).
                let doc = Doc::new();
                // Take previous_sv from empty doc so the delta includes meta.path + content.
                let previous_sv = doc.transact().state_vector();
                write_meta_path(&doc, rel_path);
                let text_ref = doc.get_or_insert_text("content");
                {
                    let mut txn = doc.transact_mut();
                    text_ref.insert(&mut txn, 0, disk_content);
                }
                if let Err(e) = save_doc(&doc, &state_path) {
                    tracing::error!("Failed to save new doc {}: {}", uuid, e);
                    continue;
                }
                let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                offline_changes.push((uuid.clone(), update));
                freshly_created.insert(uuid.clone());
            }
        }

        // ---- Phase 6: Truly new files (no matching orphan) ------------------

        for (rel_path, disk_content) in unmatched_new {
            let uuid = self.get_or_create_uuid(&rel_path);
            let state_path = self.get_state_path_for_uuid(&uuid);

            if let Some(parent) = state_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            let doc = Doc::new();
            // Take previous_sv from empty doc so the delta includes meta.path + content.
            let previous_sv = doc.transact().state_vector();
            write_meta_path(&doc, &rel_path);
            let text_ref = doc.get_or_insert_text("content");
            {
                let mut txn = doc.transact_mut();
                text_ref.insert(&mut txn, 0, &disk_content);
            }
            if let Err(e) = save_doc(&doc, &state_path) {
                tracing::error!("Failed to save new doc {}: {}", uuid, e);
                continue;
            }
            let update = doc.transact().encode_state_as_update_v1(&previous_sv);
            offline_changes.push((uuid.clone(), update));
            freshly_created.insert(uuid.clone());
        }

        // ---- Phase 7: Offline deletions (remaining orphaned UUIDs) ----------

        for orphan in orphaned {
            if !orphan.crdt_content.is_empty() {
                let state_path = self.get_state_path_for_uuid(&orphan.uuid);
                if let Ok(doc) = load_doc(&state_path) {
                    let text_ref = doc.get_or_insert_text("content");
                    let previous_sv = doc.transact().state_vector();
                    apply_diff_to_yrs(&doc, &text_ref, &orphan.crdt_content, "");
                    if let Err(e) = save_doc(&doc, &state_path) {
                        tracing::error!(
                            "Failed to save offline deletion for {}: {}",
                            orphan.uuid,
                            e
                        );
                        continue;
                    }
                    let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                    offline_changes.push((orphan.uuid.clone(), update));
                }
            }
            // Remove from path_map (file is gone)
            self.path_map.remove_by_path(&orphan.old_rel_path);
        }

        // Persist the (possibly updated) path_map.
        if let Err(e) = self.path_map.save() {
            tracing::error!("Failed to save path_map: {}", e);
        }

        Ok((offline_changes, freshly_created))
    }

    /// List all known UUIDs from the local .syncline storage (by reading .bin filenames).
    pub fn list_doc_ids(&self) -> Result<Vec<String>> {
        let mut docs = Vec::new();
        if !self.syncline_dir.exists() {
            return Ok(docs);
        }
        for entry in WalkDir::new(&self.syncline_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file()
                && path.extension().and_then(|s| s.to_str()) == Some("bin")
                && let Ok(rel) = path.strip_prefix(&self.syncline_dir)
            {
                let rel_str = rel.to_string_lossy();
                if let Some(doc_id) = rel_str.strip_suffix(".bin") {
                    docs.push(doc_id.to_string());
                }
            }
        }
        Ok(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    #[cfg(unix)]
    fn test_issue_15_symlinked_path_deletion() {
        use std::os::unix::fs::symlink;

        let root_dir = tempdir().unwrap();
        // create a real dir inside root_dir
        let real_dir = root_dir.path().join("real_dir");
        fs::create_dir(&real_dir).unwrap();

        // create a symlink pointing to real_dir
        let symlink_dir = root_dir.path().join("symlink_dir");
        symlink(&real_dir, &symlink_dir).unwrap();

        // create LocalState using the symlink directory
        let state = LocalState::new(&symlink_dir, Some("test-client".to_string()));

        // Assert that root_dir was canonicalized (pointing to real_dir)
        assert_eq!(state.root_dir, real_dir.canonicalize().unwrap());
        assert_eq!(state.raw_root_dir, symlink_dir);

        // create a file using the symlink path
        let deleted_file = symlink_dir.join("deleted_file.md");

        // get_doc_id for deleted_file should work because of the raw_root_dir fallback
        let rel_path = state
            .get_doc_id(&deleted_file)
            .expect("get_doc_id should succeed for a deleted file under a symlink");
        assert_eq!(rel_path, "deleted_file.md");
    }

    #[test]
    fn test_bootstrap_offline_changes() {
        let dir = tempdir().unwrap();
        let mut state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let file1 = dir.path().join("file1.md");
        fs::write(&file1, "Hello World").unwrap();

        // 1. First run, file is new
        let (changed, freshly) = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed.len(), 1);
        // doc_id is now a UUID string
        assert!(
            uuid::Uuid::parse_str(&changed[0].0).is_ok(),
            "doc_id should be a valid UUID, got: {}",
            changed[0].0
        );
        assert_eq!(freshly.len(), 1);
        assert!(
            uuid::Uuid::parse_str(freshly.iter().next().unwrap()).is_ok(),
            "freshly_created entry should be a UUID"
        );

        // 2. Second run, no changes
        let (changed_none, freshly_none) = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed_none.len(), 0);
        assert!(freshly_none.is_empty(), "No new files, freshly_created should be empty");

        // 3. Third run, offline modification
        fs::write(&file1, "Hello CRDT World!").unwrap();
        let (changed_mod, freshly_mod) = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed_mod.len(), 1);
        assert!(
            uuid::Uuid::parse_str(&changed_mod[0].0).is_ok(),
            "modified doc_id should be a UUID"
        );
        assert!(
            freshly_mod.is_empty(),
            "Modified existing file should NOT be in freshly_created"
        );

        // Verify underlying storage represents the change
        let rel_path = state.get_doc_id(&file1).unwrap();
        let uuid = state.path_map.get_uuid(&rel_path).unwrap().to_string();
        let state_path = state.get_state_path_for_uuid(&uuid);
        let doc = crate::client::storage::load_doc(&state_path).unwrap();
        let text_ref = doc.get_or_insert_text("content");
        {
            // Scope txn so it drops before read_meta_path (which calls get_or_insert_map →
            // transact_mut internally — that would conflict with an active read lock).
            let txn = doc.transact();
            assert_eq!(text_ref.get_string(&txn), "Hello CRDT World!");
        }
        // meta.path should be set correctly
        assert_eq!(read_meta_path(&doc).as_deref(), Some("file1.md"));
    }

    #[test]
    fn test_bootstrap_rename_detection() {
        let dir = tempdir().unwrap();
        let mut state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let file1 = dir.path().join("original.md");
        fs::write(&file1, "Rename me").unwrap();

        // Bootstrap first time — assigns a UUID
        state.bootstrap_offline_changes().unwrap();
        let uuid = state.path_map.get_uuid("original.md").unwrap().to_string();

        // Simulate offline rename (no content change)
        let file2 = dir.path().join("renamed.md");
        fs::rename(&file1, &file2).unwrap();

        // Bootstrap again — should detect rename via content matching
        let (changed, freshly) = state.bootstrap_offline_changes().unwrap();

        // UUID should be preserved (rename, not new file)
        assert!(
            freshly.is_empty(),
            "Renamed file should NOT be in freshly_created (UUID preserved)"
        );
        assert_eq!(
            state.path_map.get_uuid("renamed.md").unwrap(),
            uuid,
            "UUID should be preserved after rename"
        );
        assert!(
            state.path_map.get_uuid("original.md").is_none(),
            "Old path should be removed from path_map"
        );
        // The rename produces an update (meta.path changed)
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, uuid);
        // meta.path inside the doc should reflect the new path
        let state_path = state.get_state_path_for_uuid(&uuid);
        let doc = crate::client::storage::load_doc(&state_path).unwrap();
        assert_eq!(read_meta_path(&doc).as_deref(), Some("renamed.md"));
    }

    #[test]
    fn test_issue_5_premature_loop_interruptions() {
        let dir = tempdir().unwrap();
        let mut state = LocalState::new(dir.path(), Some("test-client".to_string()));

        fs::create_dir_all(&state.syncline_dir).unwrap();

        // Create a/file1.md only (file2 comes later so it's "new" in the second bootstrap).
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir(&dir_a).unwrap();
        fs::create_dir(&dir_b).unwrap();

        let file1 = dir_a.join("file1.md");
        fs::write(&file1, "Valid content A").unwrap();

        // First bootstrap: creates file1.bin in the flat .syncline/data/ dir.
        state.bootstrap_offline_changes().unwrap();

        let rel1 = state.get_doc_id(&file1).unwrap();
        let uuid1 = state.path_map.get_uuid(&rel1).unwrap().to_string();
        let state_path1 = state.get_state_path_for_uuid(&uuid1);

        // Modify file1 on disk so the second bootstrap will attempt to update its .bin.
        fs::write(&file1, "Modified content A").unwrap();

        // Now create file2 so it's a brand-new file for the second bootstrap.
        let file2 = dir_b.join("file2.md");
        fs::write(&file2, "Valid content B").unwrap();

        // Make file1.bin specifically read-only so save_doc fails for file1.
        // The .syncline/data/ directory itself remains writable, so file2.bin can be created.
        let mut perms = fs::metadata(&state_path1).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&state_path1, perms.clone()).unwrap();

        let changed = state.bootstrap_offline_changes();

        // Cleanup permissions so tempdir can be deleted properly
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&state_path1, perms).unwrap();

        // Issue 5: Loop aborted early and returned Err on `save_doc` using `?`!
        assert!(
            changed.is_ok(),
            "Issue 5: Loop aborted early and returned Err!"
        );
        let (docs, _) = changed.unwrap();
        // b/file2.md should still have been processed despite file1 failing
        let rel2 = "b/file2.md";
        let uuid2 = state.path_map.get_uuid(rel2).unwrap().to_string();
        assert!(
            docs.iter().any(|(id, _)| *id == uuid2),
            "file2.md wasn't processed due to loop abort!"
        );
    }

    #[test]
    fn test_list_doc_ids() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path(), Some("test-client".to_string()));

        // Empty dir, should return empty list
        let docs = state.list_doc_ids().unwrap();
        assert!(docs.is_empty());

        // Create syncline data with some UUID-named bin files
        fs::create_dir_all(&state.syncline_dir).unwrap();
        let uuid1 = uuid::Uuid::new_v4().to_string();
        let uuid2 = uuid::Uuid::new_v4().to_string();
        let uuid3 = uuid::Uuid::new_v4().to_string();
        let doc1 = state.syncline_dir.join(format!("{}.bin", uuid1));
        let doc2 = state.syncline_dir.join(format!("{}.bin", uuid2));
        let doc3 = state.syncline_dir.join(format!("{}.bin", uuid3));
        let ignored = state.syncline_dir.join("ignored.txt");

        fs::write(&doc1, vec![]).unwrap();
        fs::write(&doc2, vec![]).unwrap();
        fs::write(&doc3, vec![]).unwrap();
        fs::write(&ignored, vec![]).unwrap();

        let docs = state.list_doc_ids().unwrap();
        assert_eq!(docs.len(), 3);
        assert!(docs.contains(&uuid1));
        assert!(docs.contains(&uuid2));
        assert!(docs.contains(&uuid3));
    }

    #[test]
    fn test_offline_deletion() {
        let dir = tempdir().unwrap();
        let mut state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let file1 = dir.path().join("file1.md");
        fs::write(&file1, "Hello World").unwrap();

        // 1. First run, file is new
        state.bootstrap_offline_changes().unwrap();
        let uuid = state.path_map.get_uuid("file1.md").unwrap().to_string();

        // 2. Delete the physical file directly
        fs::remove_file(&file1).unwrap();

        // 3. Run bootstrap again, it should detect the offline deletion
        let (changed, freshly) = state.bootstrap_offline_changes().unwrap();

        // Should have one change corresponding to the empty string
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, uuid);
        assert!(freshly.is_empty(), "Deleted file should not be in freshly_created");

        // path_map should no longer have file1.md
        assert!(
            state.path_map.get_uuid("file1.md").is_none(),
            "Deleted file should be removed from path_map"
        );

        // Verify underlying storage represents the deletion (empty string)
        let state_path = state.get_state_path_for_uuid(&uuid);
        let doc = crate::client::storage::load_doc(&state_path).unwrap();
        let text_ref = doc.get_or_insert_text("content");
        let txn = doc.transact();
        assert_eq!(text_ref.get_string(&txn), "");
    }

    #[test]
    fn test_client_name_persistence() {
        let dir = tempdir().unwrap();

        // First instantiation without override: generates a name
        let state1 = LocalState::new(dir.path(), None);
        let name1 = state1.client_name.clone();
        assert!(!name1.is_empty());

        // Second instantiation without override: reuses the saved name
        let state2 = LocalState::new(dir.path(), None);
        assert_eq!(state2.client_name, name1, "Client name should be stable across restarts");

        // Third instantiation with explicit override: uses that name
        let state3 = LocalState::new(dir.path(), Some("my-laptop".to_string()));
        assert_eq!(state3.client_name, "my-laptop");

        // Fourth instantiation without override: now uses the saved override
        let state4 = LocalState::new(dir.path(), None);
        assert_eq!(state4.client_name, "my-laptop");
    }
}
