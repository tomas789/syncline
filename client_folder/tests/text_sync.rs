mod common;
use common::TestCluster;

/// Goal: Verify text edits propagate immediately between online clients.
#[test]
fn simple_propagation() {
    let mut cluster = TestCluster::new("simple_prop");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    // Client A creates note.md
    cluster.write_file("Alice", "note.md", "Hello");
    cluster.wait_sync();

    // Assert Client B receives it
    assert!(cluster.file_exists("Bob", "note.md"));
    assert_eq!(cluster.read_file("Bob", "note.md"), "Hello");

    // Client B modifies it
    cluster.write_file("Bob", "note.md", "Hello World");
    cluster.wait_sync();

    // Assert Client A receives update
    assert_eq!(cluster.read_file("Alice", "note.md"), "Hello World");
}

/// Goal: Verify CRDT usage handles concurrent online edits without data loss.
#[test]
fn simultaneous_online_edits() {
    let mut cluster = TestCluster::new("simultaneous_edits");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    // Initial state
    cluster.write_file("Alice", "note.md", "Line 1\n");
    cluster.wait_sync();
    assert_eq!(cluster.read_file("Bob", "note.md"), "Line 1\n");

    // Simultaneous edits
    // We can't truly do "simultaneous" with process interaction easily,
    // but we can write to files closely together.
    // The filesystem watcher might pick them up slightly apart, but CRDT should handle it.

    cluster.write_file("Alice", "note.md", "Line 1\nClient A update");
    cluster.write_file("Bob", "note.md", "Line 1\nClient B update");

    cluster.wait_sync();

    let content_a = cluster.read_file("Alice", "note.md");
    let content_b = cluster.read_file("Bob", "note.md");

    // Both must converge to the same content
    assert_eq!(content_a, content_b, "Clients did not converge");

    // Check that no data was lost (both updates present)
    assert!(content_a.contains("Client A update"), "A's update lost");
    assert!(content_a.contains("Client B update"), "B's update lost");
    assert!(content_a.contains("Line 1"), "Original content lost");
}
