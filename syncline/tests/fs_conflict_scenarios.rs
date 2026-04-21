//! Conflict scenarios beyond the concurrent-edit case already covered in
//! e2e.rs: delete-vs-modify, rename-vs-rename-to-different-name,
//! offline-rename-both-sides, binary LWW, etc.

mod common;

use common::{wait_for_convergence, wait_until, TestEnv};
use std::fs;
use std::time::Duration;

/// Client A deletes a file while offline; Client B modifies the same file.
/// Both reconnect. Policy question: does the modification resurrect the
/// file, or does the deletion win? This test asserts modification-wins
/// (keep user's edits), which is the safer policy for an Obsidian vault.
#[tokio::test]
async fn test_delete_vs_modify_offline_modification_wins() {
    let mut env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("contested.md");
    fs::write(&path_a, "original").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    env.clients[0].kill().await.unwrap();
    env.clients[1].kill().await.unwrap();

    // A deletes, B modifies.
    fs::remove_file(&path_a).unwrap();
    let path_b = env.client_path(1).join("contested.md");
    fs::write(&path_b, "modified before reconnect").unwrap();

    env.restart_client(1).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    env.restart_client(0).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(15)).await,
        "Clients should converge on a single outcome"
    );

    // Assertion: safest outcome is modification-wins — user's edit preserved.
    let c0 = fs::read_to_string(env.client_path(0).join("contested.md")).ok();
    let c1 = fs::read_to_string(env.client_path(1).join("contested.md")).ok();
    assert_eq!(
        c0.as_deref(),
        Some("modified before reconnect"),
        "Client 0 should have the modification"
    );
    assert_eq!(c1.as_deref(), Some("modified before reconnect"));
}

/// Both clients rename the same file to different names while offline.
/// One rename must win; the other's name becomes a conflict artifact.
#[tokio::test]
async fn test_rename_vs_rename_different_names() {
    let mut env = TestEnv::new(2).await;

    let original_a = env.client_path(0).join("origin.md");
    fs::write(&original_a, "shared").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    env.clients[0].kill().await.unwrap();
    env.clients[1].kill().await.unwrap();

    fs::rename(&original_a, env.client_path(0).join("name_from_a.md")).unwrap();
    fs::rename(
        env.client_path(1).join("origin.md"),
        env.client_path(1).join("name_from_b.md"),
    )
    .unwrap();

    env.restart_client(0).await;
    env.restart_client(1).await;
    tokio::time::sleep(Duration::from_secs(10)).await;

    // After convergence: both clients agree on a single winner. The other
    // name either doesn't exist or exists as a conflict-marked copy.
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await,
        "Clients must converge on a single winning rename"
    );

    let origin_a = env.client_path(0).join("origin.md");
    let origin_b = env.client_path(1).join("origin.md");
    assert!(
        !origin_a.exists() && !origin_b.exists(),
        "Original name should be gone on both clients"
    );
}

/// Concurrent offline binary modifications: both clients edit the same
/// binary file while offline. Binary merge uses Last-Write-Wins on
/// `meta.blob_hash` — one version must win, and the other must be
/// preserved as a conflict file (per PROTOCOL.md).
#[tokio::test]
async fn test_concurrent_binary_edits_preserves_both() {
    let mut env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("shared.bin");
    fs::write(&path_a, [0x01u8, 0x02, 0x03].as_slice()).unwrap();
    assert!(
        wait_until(Duration::from_secs(15), || env
            .client_path(1)
            .join("shared.bin")
            .exists())
        .await
    );

    env.clients[0].kill().await.unwrap();
    env.clients[1].kill().await.unwrap();

    fs::write(&path_a, [0xAAu8, 0xBB].as_slice()).unwrap();
    fs::write(
        env.client_path(1).join("shared.bin"),
        [0xCCu8, 0xDD, 0xEE].as_slice(),
    )
    .unwrap();

    env.restart_client(0).await;
    env.restart_client(1).await;
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Per PROTOCOL.md: "both versions are kept with distinct names".
    let files_a = common::collect_user_files(env.client_path(0));
    let files_b = common::collect_user_files(env.client_path(1));
    assert_eq!(
        files_a.len(),
        2,
        "Client A should have original + conflict copy, got: {:?}",
        files_a
    );
    assert_eq!(
        files_b.len(),
        2,
        "Client B should have original + conflict copy, got: {:?}",
        files_b
    );
}

/// Three-way convergence: three clients editing different files. Everything
/// must end up on all three.
#[tokio::test]
async fn test_three_client_fanout() {
    let env = TestEnv::new(3).await;

    fs::write(env.client_path(0).join("from_a.md"), "A").unwrap();
    fs::write(env.client_path(1).join("from_b.md"), "B").unwrap();
    fs::write(env.client_path(2).join("from_c.md"), "C").unwrap();

    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await,
        "Three clients should converge"
    );

    for idx in 0..3 {
        assert_eq!(
            fs::read_to_string(env.client_path(idx).join("from_a.md")).unwrap(),
            "A"
        );
        assert_eq!(
            fs::read_to_string(env.client_path(idx).join("from_b.md")).unwrap(),
            "B"
        );
        assert_eq!(
            fs::read_to_string(env.client_path(idx).join("from_c.md")).unwrap(),
            "C"
        );
    }
}

/// A file deleted on one client while a third client is offline: when the
/// third client reconnects, it should see the deletion too.
#[tokio::test]
async fn test_delete_propagates_to_late_reconnecting_client() {
    let mut env = TestEnv::new(3).await;

    let path_a = env.client_path(0).join("shared.md");
    fs::write(&path_a, "shared content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(15)).await);

    // Take client 2 offline.
    env.clients[2].kill().await.unwrap();

    // Client 0 deletes while 2 is offline.
    fs::remove_file(&path_a).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    // Client 1 should see the delete propagate immediately.
    assert!(
        wait_until(Duration::from_secs(10), || !env
            .client_path(1)
            .join("shared.md")
            .exists())
        .await,
        "Client 1 should see the deletion"
    );

    // Now bring client 2 back online.
    env.restart_client(2).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        wait_until(Duration::from_secs(15), || !env
            .client_path(2)
            .join("shared.md")
            .exists())
        .await,
        "Client 2, upon reconnection, should receive the delete"
    );
}

/// Two clients create files with the same name but different content while
/// BOTH are online (no pre-existing content). Both files must sync with
/// distinct names. Different from `test_both_offline_same_name_conflict` —
/// here the race is inside a single debounce window, not across a connect.
#[tokio::test]
async fn test_simultaneous_online_create_same_path() {
    let env = TestEnv::new(2).await;

    fs::write(env.client_path(0).join("same.md"), "from A").unwrap();
    // Write on B within the same watcher batch window.
    fs::write(env.client_path(1).join("same.md"), "from B").unwrap();

    tokio::time::sleep(Duration::from_secs(10)).await;

    // Both clients should have two docs total: one winner, one conflict copy.
    let files_a = common::collect_user_files(env.client_path(0));
    let files_b = common::collect_user_files(env.client_path(1));

    assert_eq!(
        files_a.len(),
        2,
        "Client A should have two files after online collision, got: {:?}",
        files_a
    );
    assert_eq!(
        files_b.len(),
        2,
        "Client B should have two files after online collision, got: {:?}",
        files_b
    );
}
