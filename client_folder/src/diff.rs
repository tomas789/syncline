use similar::{ChangeTag, TextDiff};
use yrs::{Doc, Text, TextRef, Transact};

pub fn apply_diff_to_yrs(doc: &Doc, text_ref: &TextRef, old_str: &str, new_str: &str) {
    let diff = TextDiff::from_chars(old_str, new_str);
    let mut txn = doc.transact_mut();

    // We need to apply changes to the TextRef taking into account its index.
    // The easiest way to apply differences from start to end without modifying offsets
    // incorrectly is to process changes while tracking the current cursor in the Yjs string.

    let mut cursor = 0;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                let len = change.value().len();
                cursor += len as u32;
            }
            ChangeTag::Delete => {
                let len = change.value().len();
                text_ref.remove_range(&mut txn, cursor, len as u32);
            }
            ChangeTag::Insert => {
                let val = change.value();
                let len = val.len();
                text_ref.insert(&mut txn, cursor, val);
                cursor += len as u32;
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
}
