mod common;
use common::TestCluster;
use std::thread;
use std::time::Duration;

/// Goal: Verify clients can connect and identify themselves.
#[test]
fn basic_connection() {
    let mut cluster = TestCluster::new("basic_conn");
    cluster.start_server();

    // Start Client A (Identity: "Alice")
    cluster.start_client("Alice");

    // Start Client B (Identity: "Bob")
    cluster.start_client("Bob");

    // Assert Server logs/state indicates both are connected
    // Use file sync as proxy for successful connection
    cluster.write_file("Alice", "conn_check.txt", "connected");
    cluster.wait_sync();

    assert!(
        cluster.file_exists("Bob", "conn_check.txt"),
        "Client B should receive file from Client A"
    );
    assert_eq!(cluster.read_file("Bob", "conn_check.txt"), "connected");
}

/// Goal: Verify a client can reconnect after a network drop.
#[test]
fn reconnection() {
    let mut cluster = TestCluster::new("reconnection");
    cluster.start_server();
    cluster.start_client("Alice");

    // Verify initial connection
    cluster.write_file("Alice", "reconn.txt", "initial");
    cluster.wait_sync();

    // Kill Server process
    println!("Stopping server...");
    cluster.stop_server();

    // Wait for client to detect disconnect
    thread::sleep(Duration::from_secs(2));

    // Start Server (should reuse port and db)
    println!("Restarting server...");
    cluster.start_server();

    // Wait for client to reconnect
    thread::sleep(Duration::from_secs(3));

    // Verify reconnection by performing sync
    cluster.write_file("Alice", "after_reconn.txt", "success");
    cluster.wait_sync();

    // Start Bob to verify sync
    cluster.start_client("Bob");
    cluster.wait_sync();

    assert!(
        cluster.file_exists("Bob", "after_reconn.txt"),
        "Bob should receive file from reconnected Alice"
    );
}
