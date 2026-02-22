use crate::client::diff::apply_diff_to_yrs;
use crate::client::storage::{load_doc, save_doc};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use yrs::{Doc, GetString, ReadTxn, Text, Transact};

pub struct LocalState {
    pub root_dir: PathBuf,
    pub raw_root_dir: PathBuf,
    pub syncline_dir: PathBuf,
}

impl LocalState {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        let raw_root_dir = root_dir.as_ref().to_path_buf();
        // Canonicalize so that path comparisons with FSEvents-reported paths
        // work correctly on macOS, where /var is a symlink to /private/var and
        // FSEvents always returns the real (canonical) path.
        let root_dir = root_dir
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| root_dir.as_ref().to_path_buf());
        let syncline_dir = root_dir.join(".syncline").join("data");
        Self {
            root_dir,
            raw_root_dir,
            syncline_dir,
        }
    }

    /// Converts a physical path to a relative doc_id (e.g., "notes/idea.md")
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

    /// Gets the path to the binary Yjs snapshot for a given doc_id
    pub fn get_state_path(&self, doc_id: &str) -> PathBuf {
        self.syncline_dir.join(format!("{}.bin", doc_id))
    }

    /// Scans the directory on startup. Compares physical files with `.syncline` state.
    /// Creates documents for missing states, applies diffs for modified files.
    /// Returns a list of `(doc_id, update)` tuples that were modified offline and need to be synced.
    pub fn bootstrap_offline_changes(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut offline_changes = Vec::new();
        let mut physical_docs = std::collections::HashSet::new();

        // Recursively walk the directory
        for entry in WalkDir::new(&self.root_dir)
            .into_iter()
            .filter_entry(|e| {
                // Always allow the root directory itself.
                if e.depth() == 0 {
                    return true;
                }
                // Exclude hidden files and directories (names starting with '.').
                let name = e.file_name().to_string_lossy();
                name != ".git" && name != ".syncline"
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // Only care about .md and .txt files
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "md" && ext != "txt" {
                continue;
            }

            let doc_id = match self.get_doc_id(path) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Failed to get doc_id for {:?}: {}", path, e);
                    continue;
                }
            };
            let state_path = self.get_state_path(&doc_id);

            // Read disk contents. If it fails (e.g. permission error), skip this file.
            let disk_content = match fs::read_to_string(path) {
                Ok(content) => content,
                Err(_) => continue,
            };

            if state_path.exists() {
                // File existed before, check if it was modified offline
                let doc = match load_doc(&state_path) {
                    Ok(doc) => doc,
                    Err(_) => continue, // Corrupted state -> we should probably recover gracefully, but skip for now
                };

                let text_ref = doc.get_or_insert_text("content");
                let yjs_content = {
                    let txn = doc.transact();
                    text_ref.get_string(&txn)
                };

                // Compare the raw text string from Yjs with the physical file string
                if disk_content != yjs_content {
                    // We found an offline edit!
                    let previous_sv = doc.transact().state_vector();
                    apply_diff_to_yrs(&doc, &text_ref, &yjs_content, &disk_content);
                    if let Err(e) = save_doc(&doc, &state_path) {
                        tracing::error!("Failed to save offline edits for {}: {}", doc_id, e);
                        continue;
                    }
                    let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                    offline_changes.push((doc_id.clone(), update));
                }
                physical_docs.insert(doc_id);
            } else {
                // New file was added offline
                let doc = Doc::new();
                let text_ref = doc.get_or_insert_text("content");
                let previous_sv = doc.transact().state_vector();
                {
                    let mut txn = doc.transact_mut();
                    text_ref.insert(&mut txn, 0, &disk_content);
                }
                if let Err(e) = save_doc(&doc, &state_path) {
                    tracing::error!("Failed to save new doc {}: {}", doc_id, e);
                    continue;
                }
                let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                offline_changes.push((doc_id.clone(), update));
                physical_docs.insert(doc_id);
            }
        }

        // Check for offline deletions
        if let Ok(known_docs) = self.list_doc_ids() {
            for doc_id in known_docs {
                if doc_id == "__index__" {
                    continue;
                }
                if !physical_docs.contains(&doc_id) {
                    let state_path = self.get_state_path(&doc_id);
                    if let Ok(doc) = load_doc(&state_path) {
                        let text_ref = doc.get_or_insert_text("content");
                        let yjs_content = text_ref.get_string(&doc.transact());
                        if !yjs_content.is_empty() {
                            tracing::info!("Found offline deletion for {}", doc_id);
                            let previous_sv = doc.transact().state_vector();
                            apply_diff_to_yrs(&doc, &text_ref, &yjs_content, "");
                            if let Err(e) = save_doc(&doc, &state_path) {
                                tracing::error!(
                                    "Failed to save offline deletion for {}: {}",
                                    doc_id,
                                    e
                                );
                                continue; // Added this continue back for correctness though not needed logically
                            }
                            let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                            offline_changes.push((doc_id, update));
                        }
                    }
                }
            }
        }

        Ok(offline_changes)
    }

    /// List all known doc_ids from the local .syncline storage
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
        let state = LocalState::new(&symlink_dir);

        // Assert that root_dir was canonicalized (pointing to real_dir)
        assert_eq!(state.root_dir, real_dir.canonicalize().unwrap());
        assert_eq!(state.raw_root_dir, symlink_dir);

        // create a file using the symlink path
        let deleted_file = symlink_dir.join("deleted_file.md");

        // get_doc_id for deleted_file should work because of the raw_root_dir fallback
        let doc_id = state
            .get_doc_id(&deleted_file)
            .expect("get_doc_id should succeed for a deleted file under a symlink");
        assert_eq!(doc_id, "deleted_file.md");
    }

    #[test]
    fn test_bootstrap_offline_changes() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path());

        let file1 = dir.path().join("file1.md");
        fs::write(&file1, "Hello World").unwrap();

        // 1. First run, file is new
        let changed = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, "file1.md");

        // 2. Second run, no changes
        let changed_none = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed_none.len(), 0);

        // 3. Third run, offline modification
        fs::write(&file1, "Hello CRDT World!").unwrap();
        let changed_mod = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed_mod.len(), 1);
        assert_eq!(changed_mod[0].0, "file1.md");

        // Verify underlying storage represents the change
        let doc_id = state.get_doc_id(&file1).unwrap();
        let state_path = state.get_state_path(&doc_id);
        let doc = load_doc(&state_path).unwrap();
        let text_ref = doc.get_or_insert_text("content");
        let txn = doc.transact();
        assert_eq!(text_ref.get_string(&txn), "Hello CRDT World!");
    }

    #[test]
    fn test_issue_5_premature_loop_interruptions() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path());

        fs::create_dir_all(&state.syncline_dir).unwrap();

        // Create a/file1.md and b/file2.md
        let dir_a = dir.path().join("a");
        let dir_b = dir.path().join("b");
        fs::create_dir(&dir_a).unwrap();
        fs::create_dir(&dir_b).unwrap();

        let file1 = dir_a.join("file1.md");
        fs::write(&file1, "Valid content A").unwrap();

        let file2 = dir_b.join("file2.md");
        fs::write(&file2, "Valid content B").unwrap();

        // Make state_path's parent directory for file1 read-only so save_doc fails.
        let doc_id1 = state.get_doc_id(&file1).unwrap();
        let state_path1 = state.get_state_path(&doc_id1);
        let parent1 = state_path1.parent().unwrap();
        fs::create_dir_all(parent1).unwrap();

        let mut perms = fs::metadata(parent1).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(parent1, perms.clone()).unwrap();

        let changed = state.bootstrap_offline_changes();

        // Cleanup permissions so tempdir can be deleted properly
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(parent1, perms).unwrap();

        // Issue 5: Loop aborted early and returned Err on `save_doc` using `?`!
        assert!(
            changed.is_ok(),
            "Issue 5: Loop aborted early and returned Err!"
        );
        let docs = changed.unwrap();
        assert!(
            docs.iter().any(|(id, _)| id == "b/file2.md"),
            "file2.md wasn't processed due to loop abort!"
        );
    }

    #[test]
    fn test_list_doc_ids() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path());

        // Empty dir, should return empty list
        let docs = state.list_doc_ids().unwrap();
        assert!(docs.is_empty());

        // Create syncline data with some bin files
        fs::create_dir_all(&state.syncline_dir).unwrap();
        let doc1 = state.syncline_dir.join("test1.md.bin");
        let doc2 = state.syncline_dir.join("test2.md.bin");
        let dir_in_state = state.syncline_dir.join("nested");
        fs::create_dir_all(&dir_in_state).unwrap();
        let doc3 = dir_in_state.join("test3.md.bin");

        // Also create a non-bin file that should be ignored
        let ignored = state.syncline_dir.join("ignored.txt");

        fs::write(&doc1, vec![]).unwrap();
        fs::write(&doc2, vec![]).unwrap();
        fs::write(&doc3, vec![]).unwrap();
        fs::write(&ignored, vec![]).unwrap();

        let mut docs = state.list_doc_ids().unwrap();
        docs.sort();

        let expected = vec![
            "test1.md".to_string(),
            "test2.md".to_string(),
            "nested/test3.md".to_string(),
        ];
        // Note: depending on the OS, path separator could be \ or /
        // Let's normalize it to just check contains instead of exact vector matching
        assert_eq!(docs.len(), 3);
        assert!(docs.iter().any(|s| s.contains("test1.md")));
        assert!(docs.iter().any(|s| s.contains("test2.md")));
        assert!(docs.iter().any(|s| s.contains("test3.md")));
    }

    #[test]
    fn test_offline_deletion() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path());

        let file1 = dir.path().join("file1.md");
        fs::write(&file1, "Hello World").unwrap();

        // 1. First run, file is new
        state.bootstrap_offline_changes().unwrap();

        // 2. Delete the physical file directly
        fs::remove_file(&file1).unwrap();

        // 3. Run bootstrap again, it should detect the offline deletion
        let changed = state.bootstrap_offline_changes().unwrap();

        // Should have one change corresponding to the empty string
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, "file1.md");

        // Verify underlying storage represents the deletion (empty string)
        let state_path = state.get_state_path("file1.md");
        let doc = load_doc(&state_path).unwrap();
        let text_ref = doc.get_or_insert_text("content");
        let txn = doc.transact();
        assert_eq!(text_ref.get_string(&txn), "");
    }
}
