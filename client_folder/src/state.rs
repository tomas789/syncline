use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use yrs::{Doc, GetString, Text, Transact};

use crate::diff::apply_diff_to_yrs;
use crate::storage::{load_doc, save_doc};

pub struct LocalState {
    pub root_dir: PathBuf,
    pub syncline_dir: PathBuf,
}

impl LocalState {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
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
        let rel = canonical.strip_prefix(&self.root_dir)?;
        Ok(rel.to_string_lossy().to_string())
    }

    /// Gets the path to the binary Yjs snapshot for a given doc_id
    pub fn get_state_path(&self, doc_id: &str) -> PathBuf {
        self.syncline_dir.join(format!("{}.bin", doc_id))
    }

    /// Scans the directory on startup. Compares physical files with `.syncline` state.
    /// Creates documents for missing states, applies diffs for modified files.
    /// Returns a list of `doc_id`s that were modified offline and need to be synced.
    pub fn bootstrap_offline_changes(&self) -> Result<Vec<String>> {
        let mut modified_docs = Vec::new();

        // Recursively walk the directory
        for entry in WalkDir::new(&self.root_dir)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                // Exclude hidden folders like .git and .syncline
                !name.starts_with(".git") && !name.starts_with(".syncline")
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
                    apply_diff_to_yrs(&doc, &text_ref, &yjs_content, &disk_content);
                    if let Err(e) = save_doc(&doc, &state_path) {
                        tracing::error!("Failed to save offline edits for {}: {}", doc_id, e);
                        continue;
                    }
                    modified_docs.push(doc_id);
                }
            } else {
                // New file was added offline
                let doc = Doc::new();
                let text_ref = doc.get_or_insert_text("content");
                {
                    let mut txn = doc.transact_mut();
                    text_ref.insert(&mut txn, 0, &disk_content);
                }
                if let Err(e) = save_doc(&doc, &state_path) {
                    tracing::error!("Failed to save new doc {}: {}", doc_id, e);
                    continue;
                }
                modified_docs.push(doc_id);
            }
        }

        Ok(modified_docs)
    }

    /// List all known doc_ids from the local .syncline storage
    pub fn list_doc_ids(&self) -> Result<Vec<String>> {
        let mut docs = Vec::new();
        if !self.syncline_dir.exists() {
            return Ok(docs);
        }
        for entry in std::fs::read_dir(&self.syncline_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("bin") {
                if let Some(stem) = path.file_stem() {
                    docs.push(stem.to_string_lossy().into_owned());
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
    fn test_bootstrap_offline_changes() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path());

        let file1 = dir.path().join("file1.md");
        fs::write(&file1, "Hello World").unwrap();

        // 1. First run, file is new
        let changed = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0], "file1.md");

        // 2. Second run, no changes
        let changed_none = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed_none.len(), 0);

        // 3. Third run, offline modification
        fs::write(&file1, "Hello CRDT World!").unwrap();
        let changed_mod = state.bootstrap_offline_changes().unwrap();
        assert_eq!(changed_mod.len(), 1);
        assert_eq!(changed_mod[0], "file1.md");

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
            docs.contains(&"b/file2.md".to_string()),
            "file2.md wasn't processed due to loop abort!"
        );
    }
}
