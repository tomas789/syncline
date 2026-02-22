use std::fs;

fn main() {
    let old_text = fs::read_to_string("test1/README.md").unwrap();
    let old_text = old_text.replace("fds\nasdf\n", ""); // go back to start
    let text1 = format!("{}fds\n", old_text);
    let text2 = format!("{}fds\nasdf\n", old_text);
    
    let diff = dissimilar::diff(&text1, &text2);
    for chunk in diff {
        match chunk {
            dissimilar::Chunk::Delete(val) => println!("Delete {:?}", val),
            dissimilar::Chunk::Insert(val) => println!("Insert {:?}", val),
            _ => ()
        }
    }
}
