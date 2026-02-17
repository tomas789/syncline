mod common;
use common::TestCluster;

/// Goal: Verify edits made while offline are synced upon reconnection.
#[test]
fn single_client_offline_edit() {
    let mut cluster = TestCluster::new("offline_edit");
    cluster.start_server();
    cluster.start_client("Alice");

    // Initial sync
    cluster.write_file("Alice", "doc.txt", "Base");
    cluster.wait_sync();

    cluster.stop_client("Alice");

    // Offline modification
    cluster.write_file("Alice", "doc.txt", "Base + Offline A");

    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.wait_sync();

    // Bob should receive A's offline changes
    assert!(cluster.file_exists("Bob", "doc.txt"));
    assert_eq!(cluster.read_file("Bob", "doc.txt"), "Base + Offline A");
}

/// Goal: Verify the system merges conflicting offline changes using CRDTs.
#[test]
fn divergent_offline_edits() {
    let mut cluster = TestCluster::new("divergent_edits");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.write_file("Alice", "story.md", "Once upon a time.");
    cluster.wait_sync();
    assert_eq!(cluster.read_file("Bob", "story.md"), "Once upon a time.");

    cluster.stop_client("Alice");
    cluster.stop_client("Bob");

    // Client A modifies: Prepend
    cluster.write_file("Alice", "story.md", "Deep in the forest, Once upon a time.");

    // Client B modifies: Append
    cluster.write_file("Bob", "story.md", "Once upon a time. The End.");

    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.wait_sync();

    let end_a = cluster.read_file("Alice", "story.md");
    let end_b = cluster.read_file("Bob", "story.md");

    // Convergence check
    assert_eq!(end_a, end_b, "Clients diverged after offline edit");

    // Content check
    assert!(end_a.contains("Deep in the forest"), "A's change lost");
    assert!(end_a.contains("The End."), "B's change lost");
}

/// Goal: Verify a client accepts updates that happened while it was offline, merging them with its own local changes.
#[test]
fn offline_while_peer_updates() {
    let mut cluster = TestCluster::new("offline_peer_update");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.write_file("Alice", "old_file.txt", "Old content");
    cluster.wait_sync();

    cluster.stop_client("Alice");

    // Bob creates new file while Alice is offline
    cluster.write_file("Bob", "new_file.txt", "New file content");
    cluster.wait_sync(); // Syncs to server

    // Alice modifies old file offline
    cluster.write_file("Alice", "old_file.txt", "Old content modified");

    cluster.start_client("Alice");
    cluster.wait_sync();

    // Alice should get new file
    assert!(cluster.file_exists("Alice", "new_file.txt"));
    assert_eq!(
        cluster.read_file("Alice", "new_file.txt"),
        "New file content"
    );

    // Bob should get Alice's update to old file
    assert_eq!(
        cluster.read_file("Bob", "old_file.txt"),
        "Old content modified"
    );
}
