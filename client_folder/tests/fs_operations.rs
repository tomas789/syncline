mod common;
use common::TestCluster;

/// Goal: Verify deletions are propagated.
#[test]
fn file_deletion_propagation() {
    let mut cluster = TestCluster::new("deletion_prop");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.write_file("Alice", "delete_me.txt", "content");
    cluster.wait_sync();

    assert!(cluster.file_exists("Bob", "delete_me.txt"));

    cluster.delete_file("Alice", "delete_me.txt");
    cluster.wait_sync();

    assert!(
        !cluster.file_exists("Bob", "delete_me.txt"),
        "Bob should delete the file"
    );
}

/// Goal: Verify renames are handled (either as move or delete+create, maintaining content).
#[test]
fn file_renaming() {
    let mut cluster = TestCluster::new("renaming");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.write_file("Alice", "old_name.txt", "Important Data");
    cluster.wait_sync();

    cluster.rename_file("Alice", "old_name.txt", "new_name.txt");
    cluster.wait_sync();

    assert!(
        cluster.file_exists("Bob", "new_name.txt"),
        "Bob should have new file"
    );
    assert!(
        !cluster.file_exists("Bob", "old_name.txt"),
        "Bob should not have old file"
    );
    assert_eq!(cluster.read_file("Bob", "new_name.txt"), "Important Data");
}

/// Goal: Ensure directory structures are synced.
#[test]
fn directory_creation() {
    let mut cluster = TestCluster::new("dir_create");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.write_file("Alice", "folder/subfolder/note.md", "Deep content");
    cluster.wait_sync();

    assert!(cluster.file_exists("Bob", "folder/subfolder/note.md"));
    assert_eq!(
        cluster.read_file("Bob", "folder/subfolder/note.md"),
        "Deep content"
    );
}

/// Goal: Verify behavior when one client deletes a file that another edits.
#[test]
fn offline_deletion_vs_online_edit() {
    let mut cluster = TestCluster::new("delete_vs_edit");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.write_file("Alice", "conflict.txt", "Initial");
    cluster.wait_sync();

    cluster.stop_client("Alice");

    // Bob edits
    cluster.write_file("Bob", "conflict.txt", "Edited by Bob");
    cluster.wait_sync(); // Sync to server

    // Alice deletes offline
    cluster.delete_file("Alice", "conflict.txt");

    cluster.start_client("Alice");
    cluster.wait_sync();

    // Current behavior assumption: Last write wins or explicit handle.
    // If edit wins -> file exists. If delete wins -> file gone.
    // Ideally, edit should win or resurrect the file (as it's a new operation on the doc).
    // Let's assert consistency first: both should have the same state.

    let a_exists = cluster.file_exists("Alice", "conflict.txt");
    let b_exists = cluster.file_exists("Bob", "conflict.txt");

    assert_eq!(a_exists, b_exists, "State should converge");
}
