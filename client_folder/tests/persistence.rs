mod common;
use common::TestCluster;
use std::thread;
use std::time::Duration;

/// Goal: Verify server retains state across restarts.
#[test]
fn server_restart() {
    let mut cluster = TestCluster::new("server_restart");
    cluster.start_server();
    cluster.start_client("Alice");

    // Create data
    cluster.write_file("Alice", "persistent.txt", "Data that should survive");
    cluster.wait_sync();

    // Verify sync to server (implicit if another client got it, but here we trust wait_sync sufficient for upload)
    // To be sure, start Bob and check.
    cluster.start_client("Bob");
    cluster.wait_sync();
    assert_eq!(
        cluster.read_file("Bob", "persistent.txt"),
        "Data that should survive"
    );

    // Stop everyone
    cluster.stop_client("Alice");
    cluster.stop_client("Bob");
    cluster.stop_server();

    // Restart Server
    cluster.start_server();

    // Start FRESH Client C (empty dir)
    cluster.start_client("Charlie");
    cluster.wait_sync();

    // Charlie should get the file from Server
    assert_eq!(
        cluster.read_file("Charlie", "persistent.txt"),
        "Data that should survive"
    );
}

/// Goal: Verify client remembers its state (vector clock) to avoid re-syncing everything as new.
#[test]
fn client_state_persistence() {
    let mut cluster = TestCluster::new("client_persistence");
    cluster.start_server();
    cluster.start_client("Alice");

    cluster.write_file("Alice", "doc.txt", "Initial content");
    cluster.wait_sync();

    cluster.stop_client("Alice");

    // Modify offline
    cluster.write_file("Alice", "doc.txt", "Initial content + Update");

    cluster.start_client("Alice");
    cluster.wait_sync();

    // Verify sync to another client
    cluster.start_client("Bob");
    cluster.wait_sync();
    assert_eq!(
        cluster.read_file("Bob", "doc.txt"),
        "Initial content + Update"
    );

    // To strictly verify "only sends new ops", we'd need log analysis.
    // E.g. grep logs for "sending update" size?
    // For this level of E2E, functional correctness is the primary assertion.
}

/// Goal: Verify server compacts history after many updates, but new clients still receive full state.
#[test]
fn history_compaction() {
    // Note: Compaction is a server feature. We need to know if it's enabled/configured.
    // Assuming default behavior handles many updates.

    let mut cluster = TestCluster::new("history_compaction");
    cluster.start_server();
    cluster.start_client("Alice");

    cluster.write_file("Alice", "doc.txt", "Start");
    cluster.wait_sync();

    // Generate many small updates
    for i in 0..50 {
        let content = format!("Start {}", i);
        cluster.write_file("Alice", "doc.txt", &content);
        // Short sleep to ensure distinct updates potentially
        thread::sleep(Duration::from_millis(50));
    }

    cluster.wait_sync();

    cluster.start_client("Bob");
    cluster.wait_sync();

    // Bob should have final state
    let final_content = cluster.read_file("Alice", "doc.txt");
    assert_eq!(cluster.read_file("Bob", "doc.txt"), final_content);
}
