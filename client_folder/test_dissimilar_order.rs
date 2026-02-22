use dissimilar::Chunk;

fn main() {
    let diffs = dissimilar::diff("cat", "bat");
    for d in diffs {
        match d {
            Chunk::Equal(v) => println!("Equal({:?})", v),
            Chunk::Insert(v) => println!("Insert({:?})", v),
            Chunk::Delete(v) => println!("Delete({:?})", v),
        }
    }
}
