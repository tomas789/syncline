use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use yrs::ReadTxn;
use yrs::Update;
use yrs::updates::decoder::Decode;
use yrs::{Doc, StateVector, Transact};

/// Serialize the entire document state to binary format.
pub fn save_doc(doc: &Doc, path: &Path) -> Result<()> {
    let txn = doc.transact();
    let update = txn.encode_state_as_update_v1(&StateVector::default());

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create parent directories")?;
    }

    fs::write(path, update).context("Failed to write document update to disk")?;
    Ok(())
}

/// Load the entire document state from binary format.
pub fn load_doc(path: &Path) -> Result<Doc> {
    let bytes = fs::read(path).context("Failed to read document update from disk")?;
    let update = Update::decode_v1(&bytes).context("Failed to decode document update")?;

    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(update)
            .map_err(|e| anyhow::anyhow!("Failed to apply update: {:?}", e))?;
    }

    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::apply_diff_to_yrs;
    use tempfile::tempdir;
    use yrs::{GetString, Text};

    #[test]
    fn test_save_and_load_doc() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.bin");

        let original_doc = Doc::new();
        let text_ref = original_doc.get_or_insert_text("content");

        {
            let mut txn = original_doc.transact_mut();
            text_ref.insert(&mut txn, 0, "Initial content");
        }

        // Save to disk
        save_doc(&original_doc, &file_path).unwrap();

        // Load from disk
        let loaded_doc = load_doc(&file_path).unwrap();
        let loaded_text_ref = loaded_doc.get_or_insert_text("content");

        {
            let txn = loaded_doc.transact();
            assert_eq!(loaded_text_ref.get_string(&txn), "Initial content");
        }

        // Let's also test modifying the loaded doc and saving again.
        apply_diff_to_yrs(
            &loaded_doc,
            &loaded_text_ref,
            "Initial content",
            "Modified content!",
        );

        save_doc(&loaded_doc, &file_path).unwrap();

        // Load once more
        let final_doc = load_doc(&file_path).unwrap();
        let final_text_ref = final_doc.get_or_insert_text("content");

        {
            let txn = final_doc.transact();
            assert_eq!(final_text_ref.get_string(&txn), "Modified content!");
        }
    }
}
