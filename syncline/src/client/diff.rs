use dissimilar::Chunk;
use yrs::{Doc, Text, TextRef, Transact};

pub fn apply_diff_to_yrs(doc: &Doc, text_ref: &TextRef, old_str: &str, new_str: &str) {
    if old_str == new_str {
        return;
    }

    let mut txn = doc.transact_mut();

    if old_str.is_empty() {
        text_ref.insert(&mut txn, 0, new_str);
        return;
    }

    if new_str.is_empty() {
        text_ref.remove_range(&mut txn, 0, old_str.len() as u32);
        return;
    }

    let diff = dissimilar::diff(old_str, new_str);

    let mut cursor = 0;

    for chunk in diff {
        match chunk {
            Chunk::Equal(val) => {
                cursor += val.len() as u32;
            }
            Chunk::Delete(val) => {
                text_ref.remove_range(&mut txn, cursor, val.len() as u32);
            }
            Chunk::Insert(val) => {
                text_ref.insert(&mut txn, cursor, val);
                cursor += val.len() as u32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{Doc, GetString, Text, Transact};

    #[test]
    fn test_apply_diff_basic() {
        let doc = Doc::new();
        let text_ref = doc.get_or_insert_text("content");

        {
            let mut txn = doc.transact_mut();
            text_ref.insert(&mut txn, 0, "Hello World");
        }

        apply_diff_to_yrs(&doc, &text_ref, "Hello World", "Hello CRDT World!");

        {
            let txn = doc.transact();
            assert_eq!(text_ref.get_string(&txn), "Hello CRDT World!");
        }
    }

    #[test]
    fn test_apply_diff_delete() {
        let doc = Doc::new();
        let text_ref = doc.get_or_insert_text("content");

        {
            let mut txn = doc.transact_mut();
            text_ref.insert(&mut txn, 0, "Hello CRDT World!");
        }

        apply_diff_to_yrs(&doc, &text_ref, "Hello CRDT World!", "Hello World");

        {
            let txn = doc.transact();
            assert_eq!(text_ref.get_string(&txn), "Hello World");
        }
    }

    #[test]
    fn test_issue_4_apply_diff_unicode() {
        let doc = Doc::new();
        let text_ref = doc.get_or_insert_text("content");

        {
            let mut txn = doc.transact_mut();
            text_ref.insert(&mut txn, 0, "🚀a");
        }

        // We change 'a' to 'b' after a 4-byte unicode character
        apply_diff_to_yrs(&doc, &text_ref, "🚀a", "🚀b");

        {
            let txn = doc.transact();
            // Issue 4: counting via `chars().count()` instead of `len()` will desync the map!
            // It will remove byte index 1 (part of the Emoji) and insert there, which panics!
            // If it manages to survive instead of panicking, it will fail this assert.
            assert_eq!(text_ref.get_string(&txn), "🚀b");
        }
    }

    #[test]
    fn test_apply_diff_equal() {
        let doc = Doc::new();
        let text_ref = doc.get_or_insert_text("content");

        {
            let mut txn = doc.transact_mut();
            text_ref.insert(&mut txn, 0, "Same");
        }

        apply_diff_to_yrs(&doc, &text_ref, "Same", "Same");

        {
            let txn = doc.transact();
            assert_eq!(text_ref.get_string(&txn), "Same");
        }
    }

    #[test]
    fn test_apply_diff_old_empty() {
        let doc = Doc::new();
        let text_ref = doc.get_or_insert_text("content");

        apply_diff_to_yrs(&doc, &text_ref, "", "New Str");

        {
            let txn = doc.transact();
            assert_eq!(text_ref.get_string(&txn), "New Str");
        }
    }

    #[test]
    fn test_apply_diff_new_empty() {
        let doc = Doc::new();
        let text_ref = doc.get_or_insert_text("content");

        {
            let mut txn = doc.transact_mut();
            text_ref.insert(&mut txn, 0, "Old Str");
        }

        apply_diff_to_yrs(&doc, &text_ref, "Old Str", "");

        {
            let txn = doc.transact();
            assert_eq!(text_ref.get_string(&txn), "");
        }
    }
}
