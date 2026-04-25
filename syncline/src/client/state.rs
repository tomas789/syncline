use crate::client::diff::apply_diff_to_yrs;
use crate::client::storage::{load_doc, save_doc};
use crate::ignore::IgnoreList;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

        // Write to temp file, fsync, then atomic rename — same pattern as
        // storage::save_doc to avoid half-written JSON on crash.
        let tmp_path = self.file_path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&tmp_path)?;
            std::io::Write::write_all(&mut file, json.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &self.file_path)?;
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
// Helpers for reading/writing meta fields from a Yrs Y.Map
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

/// Write the "path" field to the "meta" Y.Map in a doc.
pub fn write_meta_path(doc: &Doc, rel_path: &str) {
    let meta: MapRef = doc.get_or_insert_map("meta");
    let mut txn = doc.transact_mut();
    meta.insert(&mut txn, "path", rel_path);
}

/// Read `meta.type` — returns "text" or "binary".
pub fn read_meta_type(doc: &Doc) -> Option<String> {
    let meta: MapRef = doc.get_or_insert_map("meta");
    let txn = doc.transact();
    match meta.get(&txn, "type") {
        Some(Out::Any(Any::String(arc))) => Some(arc.to_string()),
        _ => None,
    }
}

/// Write `meta.type` — "text" or "binary".
pub fn write_meta_type(doc: &Doc, file_type: &str) {
    let meta: MapRef = doc.get_or_insert_map("meta");
    let mut txn = doc.transact_mut();
    meta.insert(&mut txn, "type", file_type);
}

/// Read `meta.blob_hash` — the SHA256 hex hash of the binary content.
pub fn read_meta_blob_hash(doc: &Doc) -> Option<String> {
    let meta: MapRef = doc.get_or_insert_map("meta");
    let txn = doc.transact();
    match meta.get(&txn, "blob_hash") {
        Some(Out::Any(Any::String(arc))) => Some(arc.to_string()),
        _ => None,
    }
}

/// Write `meta.blob_hash` — the SHA256 hex hash of the binary content.
pub fn write_meta_blob_hash(doc: &Doc, hash: &str) {
    let meta: MapRef = doc.get_or_insert_map("meta");
    let mut txn = doc.transact_mut();
    meta.insert(&mut txn, "blob_hash", hash);
}

// ---------------------------------------------------------------------------
// File classification
// ---------------------------------------------------------------------------

/// Text extensions that use CRDT text synchronization.
const TEXT_EXTENSIONS: &[&str] = &["md", "txt"];

/// Returns true if the file should be treated as binary (blob-based sync).
pub fn is_binary_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    !TEXT_EXTENSIONS.contains(&ext)
}

/// Compute SHA256 hex digest of a byte slice.
pub fn sha256_hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

// ---------------------------------------------------------------------------
// BlobChange — returned by bootstrap for binary files that need uploading
// ---------------------------------------------------------------------------

/// Represents a binary file whose blob content needs to be uploaded to the server.
pub struct BlobChange {
    /// The UUID of the document.
    pub uuid: String,
    /// The raw binary content.
    pub data: Vec<u8>,
    /// The SHA256 hex hash of the data.
    pub hash: String,
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
    /// - A list of `BlobChange` entries for binary files whose blobs need uploading.
    pub fn bootstrap_offline_changes(
        &mut self,
    ) -> Result<(Vec<(String, Vec<u8>)>, HashSet<String>, Vec<BlobChange>)> {
        let mut offline_changes = Vec::new();
        let mut freshly_created: HashSet<String> = HashSet::new();
        let mut blob_changes: Vec<BlobChange> = Vec::new();

        // ---- Phase 1: Collect all disk files --------------------------------
        enum Content {
            Text(String),
            Binary { data: Vec<u8>, hash: String },
        }
        struct DiskFile {
            rel_path: String,
            content: Content,
        }
        let mut disk_files: Vec<DiskFile> = Vec::new();

        let ignore = IgnoreList::load(&self.root_dir);
        let root_for_filter = self.root_dir.clone();
        let ignore_for_filter = ignore.clone();
        for entry in WalkDir::new(&self.root_dir)
            .into_iter()
            .filter_entry(move |e| {
                if e.depth() == 0 {
                    return true;
                }
                let Ok(rel) = e.path().strip_prefix(&root_for_filter) else {
                    return true;
                };
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let is_dir = e.file_type().is_dir();
                !ignore_for_filter.is_ignored(&rel_str, is_dir)
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let rel_path = match self.get_doc_id(path) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Failed to get rel_path for {:?}: {}", path, e);
                    continue;
                }
            };

            // Defensive re-check — `filter_entry` is the primary gate, but
            // canonicalization quirks could yield a path the walker accepted
            // but which the ignore list would reject as a string.
            if ignore.is_ignored(&rel_path, false) {
                continue;
            }

            let content = if is_binary_file(path) {
                match fs::read(path) {
                    Ok(data) => {
                        let hash = sha256_hash(&data);
                        Content::Binary { data, hash }
                    }
                    Err(_) => continue,
                }
            } else {
                match fs::read_to_string(path) {
                    Ok(c) => Content::Text(c),
                    Err(_) => continue,
                }
            };

            disk_files.push(DiskFile { rel_path, content });
        }

        // ---- Phase 2: Separate known vs new files ---------------------------
        // known: rel_path already in path_map
        // new: not in path_map → potential new file or rename target

        struct KnownFile {
            rel_path: String,
            content: Content,
            uuid: String,
        }
        let mut known_files: Vec<KnownFile> = Vec::new();
        let mut new_files: Vec<DiskFile> = Vec::new();

        for df in disk_files {
            if let Some(uuid) = self.path_map.get_uuid(&df.rel_path) {
                known_files.push(KnownFile {
                    rel_path: df.rel_path,
                    content: df.content,
                    uuid: uuid.to_string(),
                });
            } else {
                new_files.push(df);
            }
        }

        // ---- Phase 3: Find orphaned UUIDs -----------------------------------
        // An orphaned UUID is one whose mapped path is not on disk (possibly renamed/deleted).

        let known_paths: HashSet<&str> =
            known_files.iter().map(|kf| kf.rel_path.as_str()).collect();

        struct Orphaned {
            uuid: String,
            old_rel_path: String,
            /// For text files: CRDT text content. For binary: empty.
            crdt_content: String,
            /// For binary files: the stored blob hash. For text: None.
            blob_hash: Option<String>,
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
                if state_path.exists()
                    && let Ok(doc) = load_doc(&state_path) {
                        let meta_type =
                            read_meta_type(&doc).unwrap_or_else(|| "text".to_string());
                        if meta_type == "binary" {
                            let blob_hash = read_meta_blob_hash(&doc);
                            orphaned.push(Orphaned {
                                uuid: uuid.clone(),
                                old_rel_path: rel_path.clone(),
                                crdt_content: String::new(),
                                blob_hash,
                            });
                        } else {
                            let text = doc.get_or_insert_text("content");
                            let crdt_content = text.get_string(&doc.transact());
                            orphaned.push(Orphaned {
                                uuid: uuid.clone(),
                                old_rel_path: rel_path.clone(),
                                crdt_content,
                                blob_hash: None,
                            });
                        }
                    }
            }
        }

        // ---- Phase 4: Content-based rename detection ------------------------
        // If a new file's disk content exactly matches an orphaned UUID's CRDT content
        // (text) or blob hash (binary), treat it as a rename.

        let mut renamed_map: HashMap<String, String> = HashMap::new(); // uuid → new_rel_path
        let mut unmatched_new: Vec<DiskFile> = Vec::new();

        for df in new_files {
            let matched = match &df.content {
                Content::Text(text) => orphaned
                    .iter()
                    .position(|o| o.blob_hash.is_none() && o.crdt_content == *text),
                Content::Binary { hash, .. } => orphaned
                    .iter()
                    .position(|o| o.blob_hash.as_deref() == Some(hash.as_str())),
            };

            if let Some(idx) = matched {
                let o = orphaned.remove(idx);
                renamed_map.insert(o.uuid.clone(), df.rel_path.clone());
                // Update path_map: remove old entry, add new one
                self.path_map.remove_by_path(&o.old_rel_path);
                self.path_map.insert(df.rel_path.clone(), o.uuid.clone());
                known_files.push(KnownFile {
                    rel_path: df.rel_path,
                    content: df.content,
                    uuid: o.uuid,
                });
            } else {
                unmatched_new.push(df);
            }
        }

        // ---- Phase 5: Process known files (including renamed ones) ----------

        for kf in &known_files {
            let state_path = self.get_state_path_for_uuid(&kf.uuid);
            let was_renamed = renamed_map.contains_key(kf.uuid.as_str());

            if state_path.exists() {
                let doc = match load_doc(&state_path) {
                    Ok(doc) => doc,
                    Err(_) => continue,
                };

                let previous_sv = doc.transact().state_vector();

                // Ensure meta.path is up to date (important for renamed files).
                let current_meta = read_meta_path(&doc);
                let meta_path_changed =
                    current_meta.as_deref() != Some(kf.rel_path.as_str());
                if meta_path_changed {
                    write_meta_path(&doc, &kf.rel_path);
                }

                match &kf.content {
                    Content::Text(disk_content) => {
                        // Ensure meta.type is set
                        if read_meta_type(&doc).is_none() {
                            write_meta_type(&doc, "text");
                        }

                        let text_ref = doc.get_or_insert_text("content");
                        let yrs_content = text_ref.get_string(&doc.transact());

                        if *disk_content != yrs_content
                            || was_renamed
                            || meta_path_changed
                        {
                            if *disk_content != yrs_content {
                                apply_diff_to_yrs(
                                    &doc,
                                    &text_ref,
                                    &yrs_content,
                                    disk_content,
                                );
                            }
                            if let Err(e) = save_doc(&doc, &state_path) {
                                tracing::error!(
                                    "Failed to save offline edits for {}: {}",
                                    kf.uuid,
                                    e
                                );
                                continue;
                            }
                            let update = doc
                                .transact()
                                .encode_state_as_update_v1(&previous_sv);
                            offline_changes.push((kf.uuid.clone(), update));
                        }
                    }
                    Content::Binary { data, hash } => {
                        write_meta_type(&doc, "binary");
                        let prev_hash = read_meta_blob_hash(&doc);
                        let hash_changed =
                            prev_hash.as_deref() != Some(hash.as_str());

                        if hash_changed || was_renamed || meta_path_changed {
                            if hash_changed {
                                write_meta_blob_hash(&doc, hash);
                            }
                            if let Err(e) = save_doc(&doc, &state_path) {
                                tracing::error!(
                                    "Failed to save binary meta for {}: {}",
                                    kf.uuid,
                                    e
                                );
                                continue;
                            }
                            let update = doc
                                .transact()
                                .encode_state_as_update_v1(&previous_sv);
                            offline_changes.push((kf.uuid.clone(), update));
                            if hash_changed {
                                blob_changes.push(BlobChange {
                                    uuid: kf.uuid.clone(),
                                    data: data.clone(),
                                    hash: hash.clone(),
                                });
                            }
                        }
                    }
                }
            } else {
                // First time this file is seen with its current UUID.
                let doc = Doc::new();
                let previous_sv = doc.transact().state_vector();
                write_meta_path(&doc, &kf.rel_path);

                match &kf.content {
                    Content::Text(disk_content) => {
                        write_meta_type(&doc, "text");
                        let text_ref = doc.get_or_insert_text("content");
                        {
                            let mut txn = doc.transact_mut();
                            text_ref.insert(&mut txn, 0, disk_content);
                        }
                    }
                    Content::Binary { data, hash } => {
                        write_meta_type(&doc, "binary");
                        write_meta_blob_hash(&doc, hash);
                        blob_changes.push(BlobChange {
                            uuid: kf.uuid.clone(),
                            data: data.clone(),
                            hash: hash.clone(),
                        });
                    }
                }

                if let Err(e) = save_doc(&doc, &state_path) {
                    tracing::error!("Failed to save new doc {}: {}", kf.uuid, e);
                    continue;
                }
                let update =
                    doc.transact().encode_state_as_update_v1(&previous_sv);
                offline_changes.push((kf.uuid.clone(), update));
                freshly_created.insert(kf.uuid.clone());
            }
        }

        // ---- Phase 6: Truly new files (no matching orphan) ------------------

        for df in unmatched_new {
            let uuid = self.get_or_create_uuid(&df.rel_path);
            let state_path = self.get_state_path_for_uuid(&uuid);

            if let Some(parent) = state_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            let doc = Doc::new();
            let previous_sv = doc.transact().state_vector();
            write_meta_path(&doc, &df.rel_path);

            match &df.content {
                Content::Text(disk_content) => {
                    write_meta_type(&doc, "text");
                    let text_ref = doc.get_or_insert_text("content");
                    {
                        let mut txn = doc.transact_mut();
                        text_ref.insert(&mut txn, 0, disk_content);
                    }
                }
                Content::Binary { data, hash } => {
                    write_meta_type(&doc, "binary");
                    write_meta_blob_hash(&doc, hash);
                    blob_changes.push(BlobChange {
                        uuid: uuid.clone(),
                        data: data.clone(),
                        hash: hash.clone(),
                    });
                }
            }

            if let Err(e) = save_doc(&doc, &state_path) {
                tracing::error!("Failed to save new doc {}: {}", uuid, e);
                continue;
            }
            let update =
                doc.transact().encode_state_as_update_v1(&previous_sv);
            offline_changes.push((uuid.clone(), update));
            freshly_created.insert(uuid.clone());
        }

        // ---- Phase 7: Offline deletions (remaining orphaned UUIDs) ----------

        for orphan in orphaned {
            let state_path = self.get_state_path_for_uuid(&orphan.uuid);
            if let Ok(doc) = load_doc(&state_path) {
                let previous_sv = doc.transact().state_vector();
                let needs_update = if orphan.blob_hash.is_some() {
                    // Binary file: clear blob_hash to signal deletion
                    let current_hash = read_meta_blob_hash(&doc);
                    if current_hash.is_some() {
                        write_meta_blob_hash(&doc, "");
                        true
                    } else {
                        false
                    }
                } else if !orphan.crdt_content.is_empty() {
                    // Text file: clear content
                    let text_ref = doc.get_or_insert_text("content");
                    apply_diff_to_yrs(
                        &doc,
                        &text_ref,
                        &orphan.crdt_content,
                        "",
                    );
                    true
                } else {
                    false
                };

                if needs_update {
                    if let Err(e) = save_doc(&doc, &state_path) {
                        tracing::error!(
                            "Failed to save offline deletion for {}: {}",
                            orphan.uuid,
                            e
                        );
                        continue;
                    }
                    let update = doc
                        .transact()
                        .encode_state_as_update_v1(&previous_sv);
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

        Ok((offline_changes, freshly_created, blob_changes))
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
        let (changed, freshly, _blobs) = state.bootstrap_offline_changes().unwrap();
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
        let (changed_none, freshly_none, _) = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed_none.len(), 0);
        assert!(freshly_none.is_empty(), "No new files, freshly_created should be empty");

        // 3. Third run, offline modification
        fs::write(&file1, "Hello CRDT World!").unwrap();
        let (changed_mod, freshly_mod, _) = state.bootstrap_offline_changes().unwrap();
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
        let (changed, freshly, _) = state.bootstrap_offline_changes().unwrap();

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
        let (docs, _, _) = changed.unwrap();
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
        let (changed, freshly, _) = state.bootstrap_offline_changes().unwrap();

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
