use yrs::{Doc, Text, Transact, GetString};

fn main() {
    let doc = Doc::new();
    let text = doc.get_or_insert_text("content");
    let mut txn = doc.transact_mut();
    text.insert(&mut txn, 0, "🚀a");
    
    // let's try to delete 'a'
    // if yrs uses characters, 'a' is at index 1.
    // if yrs uses bytes, 'a' is at index 4.
    text.remove_range(&mut txn, 1, 1);
    
    let str = text.get_string(&txn);
    println!("Deleted index 1 of '🚀a': {:?}", str);
}
