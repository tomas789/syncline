//! Directory-level sync tests. Syncline's model is file-only (no explicit
//! directory documents), which means directory operations are emergent:
//! creating a directory has no direct effect, deleting a directory is a
//! batch file-delete, renaming a directory is a batch file-rename.
//!
//! These tests codify user expectations around those operations even when
//! the protocol has no direct support — documenting the gap is the point.

mod common;

use common::{wait_for_convergence, wait_until, TestEnv};
use std::fs;
use std::time::Duration;

/// Creating an empty directory will not be synced — Syncline has no concept
/// of a directory document. Expected to fail until directory-aware sync is
/// added.
#[tokio::test]
async fn test_create_empty_directory_syncs() {
    let env = TestEnv::new(2).await;

    let dir_a = env.client_path(0).join("empty_folder");
    fs::create_dir(&dir_a).unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    let dir_b = env.client_path(1).join("empty_folder");
    assert!(
        dir_b.exists() && dir_b.is_dir(),
        "Empty directory should sync to peer (known gap: no directory sync)"
    );
}

/// Creating a directory with files: files should sync. The directory is
/// implicit (created as a side effect of writing the files).
#[tokio::test]
async fn test_create_directory_with_files_syncs() {
    let env = TestEnv::new(2).await;

    let dir_a = env.client_path(0).join("folder");
    fs::create_dir(&dir_a).unwrap();
    fs::write(dir_a.join("a.md"), "alpha").unwrap();
    fs::write(dir_a.join("b.md"), "beta").unwrap();

    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    let dir_b = env.client_path(1).join("folder");
    assert!(dir_b.exists());
    assert_eq!(fs::read_to_string(dir_b.join("a.md")).unwrap(), "alpha");
    assert_eq!(fs::read_to_string(dir_b.join("b.md")).unwrap(), "beta");
}

/// Deleting a directory containing files (recursive remove) should result in
/// all files gone on the peer AND ideally the directory itself removed.
#[tokio::test]
async fn test_delete_directory_removes_all_files_on_peer() {
    let env = TestEnv::new(2).await;

    let dir_a = env.client_path(0).join("doomed");
    fs::create_dir(&dir_a).unwrap();
    fs::write(dir_a.join("one.md"), "1").unwrap();
    fs::write(dir_a.join("two.md"), "2").unwrap();
    fs::write(dir_a.join("three.md"), "3").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    fs::remove_dir_all(&dir_a).unwrap();

    let dir_b = env.client_path(1).join("doomed");
    let ok = wait_until(Duration::from_secs(15), || {
        !dir_b.join("one.md").exists()
            && !dir_b.join("two.md").exists()
            && !dir_b.join("three.md").exists()
    })
    .await;
    assert!(ok, "All files inside deleted dir should be gone on peer");
}

/// Deleting an empty directory: trivially "works" in Syncline because empty
/// dirs aren't tracked. But if the dir became empty only after syncing a
/// delete, the stale empty dir remains on peer — asserts cleanup.
#[tokio::test]
async fn test_empty_directory_cleaned_after_all_files_deleted() {
    let env = TestEnv::new(2).await;

    let dir_a = env.client_path(0).join("shortlived");
    fs::create_dir(&dir_a).unwrap();
    fs::write(dir_a.join("only.md"), "content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    fs::remove_file(dir_a.join("only.md")).unwrap();

    tokio::time::sleep(Duration::from_secs(5)).await;
    let dir_b = env.client_path(1).join("shortlived");
    assert!(
        !dir_b.exists(),
        "Directory whose only file was deleted should be cleaned on peer \
         (known gap: empty dirs left behind)"
    );
}

/// Rename of a directory with files: notify fires delete+create for each
/// file. Content-hash rename detection must match each individually.
#[tokio::test]
async fn test_rename_directory_with_files() {
    let env = TestEnv::new(2).await;

    let src = env.client_path(0).join("old_name");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("a.md"), "alpha content").unwrap();
    fs::write(src.join("b.md"), "beta content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    let dst = env.client_path(0).join("new_name");
    fs::rename(&src, &dst).unwrap();

    let peer_old = env.client_path(1).join("old_name");
    let peer_new = env.client_path(1).join("new_name");
    let ok = wait_until(Duration::from_secs(15), || {
        !peer_old.exists()
            && peer_new.join("a.md").exists()
            && peer_new.join("b.md").exists()
    })
    .await;
    assert!(ok, "Directory rename should move all files to new prefix on peer");
}

/// Move a directory into a different parent. All contained files should
/// follow.
#[tokio::test]
async fn test_move_directory_to_new_parent() {
    let env = TestEnv::new(2).await;

    let outer_a = env.client_path(0).join("outer");
    let inner_a = outer_a.join("inner");
    fs::create_dir_all(&inner_a).unwrap();
    fs::write(inner_a.join("f.md"), "file inside inner").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Move `inner` up to the root of the vault.
    let moved_a = env.client_path(0).join("inner");
    fs::rename(&inner_a, &moved_a).unwrap();

    let peer_inner_old = env.client_path(1).join("outer/inner/f.md");
    let peer_inner_new = env.client_path(1).join("inner/f.md");
    let ok = wait_until(Duration::from_secs(15), || {
        !peer_inner_old.exists() && peer_inner_new.exists()
    })
    .await;
    assert!(ok, "Directory move should relocate all contained files on peer");
}

/// Deeply nested directory creation (5+ levels) should not stall or choke
/// the watcher batching.
#[tokio::test]
async fn test_deep_nested_directory_creation() {
    let env = TestEnv::new(2).await;

    let deep = env.client_path(0).join("l1/l2/l3/l4/l5/l6");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("leaf.md"), "bottom").unwrap();

    let peer = env.client_path(1).join("l1/l2/l3/l4/l5/l6/leaf.md");
    let ok = wait_until(Duration::from_secs(15), || peer.exists()).await;
    assert!(ok, "Deeply nested file should sync");
    assert_eq!(fs::read_to_string(&peer).unwrap(), "bottom");
}
