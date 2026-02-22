use yrs::{Doc, StateVector, Transact, Update};

fn main() {
    let doc = Doc::new();
    let txn = doc.transact();
    let update = txn.encode_state_as_update_v1(&StateVector::default());
    println!("Update length: {}", update.len());
    println!("Update bytes: {:?}", update);
}
