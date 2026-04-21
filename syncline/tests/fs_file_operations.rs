//! File-level sync tests: create, modify, delete, rename, move for text and
//! binary files. Each test exercises the behavior across two live clients to
//! verify the sync protocol propagates the operation end-to-end.
//!
//! Tests here that fail document real protocol gaps — binary deletion, move
//! across directories, truncate-to-empty, etc. They are written against the
//! intended user-visible semantics, not the current implementation.

mod common;

use common::{wait_for_convergence, wait_until, TestEnv};
use std::fs;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// Creating an empty text file on A should result in an empty file on B.
/// Edge case: content-based rename detection assumes non-trivial content.
#[tokio::test]
async fn test_create_empty_text_file_syncs() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("empty.md");
    fs::write(&path_a, "").unwrap();

    let path_b = env.client_path(1).join("empty.md");
    let ok = wait_until(Duration::from_secs(10), || path_b.exists()).await;
    assert!(ok, "Empty text file should appear on client B");
    assert_eq!(fs::read_to_string(&path_b).unwrap(), "");
}

/// Creating an empty binary file (e.g. `touch foo.png`) should sync.
#[tokio::test]
async fn test_create_empty_binary_file_syncs() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("empty.png");
    fs::write(&path_a, b"").unwrap();

    let path_b = env.client_path(1).join("empty.png");
    let ok = wait_until(Duration::from_secs(15), || path_b.exists()).await;
    assert!(ok, "Empty binary file should appear on client B");
    assert_eq!(fs::read(&path_b).unwrap(), b"");
}

/// A multi-level deep path must sync and the parent directories must be
/// created on the receiving client.
#[tokio::test]
async fn test_create_nested_path_creates_parent_dirs() {
    let env = TestEnv::new(2).await;

    let deep = env.client_path(0).join("a/b/c/d/deep.md");
    fs::create_dir_all(deep.parent().unwrap()).unwrap();
    fs::write(&deep, "deep content").unwrap();

    let mirror = env.client_path(1).join("a/b/c/d/deep.md");
    let ok = wait_until(Duration::from_secs(10), || mirror.exists()).await;
    assert!(ok, "Deeply nested file should sync and create parents");
    assert_eq!(fs::read_to_string(&mirror).unwrap(), "deep content");
}

// ---------------------------------------------------------------------------
// Modify
// ---------------------------------------------------------------------------

/// Overwriting with shorter content (truncate) should propagate.
#[tokio::test]
async fn test_modify_truncate_text_syncs() {
    let env = TestEnv::new(2).await;
    let path_a = env.client_path(0).join("trunc.md");
    fs::write(&path_a, "a long initial piece of content here").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    fs::write(&path_a, "short").unwrap();
    let path_b = env.client_path(1).join("trunc.md");
    let ok = wait_until(Duration::from_secs(10), || {
        fs::read_to_string(&path_b).map(|c| c == "short").unwrap_or(false)
    })
    .await;
    assert!(ok, "Truncated content should propagate to client B");
}

/// Append-style edits (common in tail-driven workflows) must sync.
#[tokio::test]
async fn test_modify_append_text_syncs() {
    let env = TestEnv::new(2).await;
    let path_a = env.client_path(0).join("append.md");
    fs::write(&path_a, "line 1\n").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    fs::write(&path_a, "line 1\nline 2\nline 3\n").unwrap();
    let path_b = env.client_path(1).join("append.md");
    let ok = wait_until(Duration::from_secs(10), || {
        fs::read_to_string(&path_b)
            .map(|c| c == "line 1\nline 2\nline 3\n")
            .unwrap_or(false)
    })
    .await;
    assert!(ok, "Appended text should propagate to client B");
}

/// Modifying a text file back to empty content must delete (or at minimum
/// empty) the file on the other client — empty content signals deletion in
/// the current protocol.
#[tokio::test]
async fn test_modify_text_to_empty_removes_on_peer() {
    let env = TestEnv::new(2).await;
    let path_a = env.client_path(0).join("erase.md");
    fs::write(&path_a, "something").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Overwrite to empty string — should be treated as deletion.
    fs::write(&path_a, "").unwrap();

    let path_b = env.client_path(1).join("erase.md");
    let ok = wait_until(Duration::from_secs(10), || {
        !path_b.exists() || fs::read_to_string(&path_b).map(|s| s.is_empty()).unwrap_or(false)
    })
    .await;
    assert!(ok, "Emptying a text file should remove or empty it on the peer");
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Online deletion of a text file should propagate as a deletion (no file on
/// the peer) rather than as an empty file. This is the primary known gap.
#[tokio::test]
async fn test_delete_text_file_online_removes_on_peer() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("delete_me.md");
    fs::write(&path_a, "hello").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    fs::remove_file(&path_a).unwrap();

    let path_b = env.client_path(1).join("delete_me.md");
    let ok = wait_until(Duration::from_secs(10), || !path_b.exists()).await;
    assert!(
        ok,
        "Deleted text file should not exist on peer (currently leaves empty file behind)"
    );
}

/// Online deletion of a binary file (png, jpg, etc.) should propagate.
/// Known gap: binary deletion is not handled by the live watcher path at all
/// — only text content clearing is broadcast.
#[tokio::test]
async fn test_delete_binary_file_online_removes_on_peer() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("bin_delete.png");
    fs::write(&path_a, b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d").unwrap();

    // Wait for binary sync
    let path_b = env.client_path(1).join("bin_delete.png");
    let ok = wait_until(Duration::from_secs(15), || path_b.exists()).await;
    assert!(ok, "Binary file should have synced before delete test");

    fs::remove_file(&path_a).unwrap();

    let gone = wait_until(Duration::from_secs(10), || !path_b.exists()).await;
    assert!(
        gone,
        "Deleted binary file should not exist on peer (known gap: binary deletion not propagated)"
    );
}

/// After deletion, the entry in `__index__` should be removed. Known gap:
/// F-6 documents that the CLI client never removes from `__index__`.
#[tokio::test]
async fn test_delete_removes_from_index() {
    use std::path::PathBuf;
    use yrs::{GetString, Transact};

    fn count_index_uuids(path: &std::path::Path) -> usize {
        let Ok(raw) = fs::read(path) else { return 0 };
        let doc = yrs::Doc::new();
        if let Ok(update) = yrs::updates::decoder::Decode::decode_v1(&raw) {
            let mut txn = doc.transact_mut();
            txn.apply_update(update);
        }
        let text = doc.get_or_insert_text("content");
        let txn = doc.transact();
        GetString::get_string(&text, &txn)
            .lines()
            .filter(|s| !s.trim().is_empty())
            .count()
    }

    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("indexed.md");
    fs::write(&path_a, "hello").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    let index_path: PathBuf = env.client_path(0).join(".syncline/data/__index__.bin");
    let before = count_index_uuids(&index_path);

    fs::remove_file(&path_a).unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;

    let after = count_index_uuids(&index_path);

    assert!(
        after < before,
        "__index__ should shrink after deletion (known gap F-6: CLI client never prunes __index__). \
         before={}, after={}",
        before,
        after
    );
}

/// Rapid create+delete within a single debounce window: the file flickers in
/// and out before the watcher fires. The peer should not end up with an
/// orphaned file.
#[tokio::test]
async fn test_rapid_create_then_delete_does_not_leak_to_peer() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("flicker.md");
    fs::write(&path_a, "transient").unwrap();
    // Shorter than the 300ms debounce — should collapse to a no-op.
    tokio::time::sleep(Duration::from_millis(50)).await;
    fs::remove_file(&path_a).unwrap();

    let path_b = env.client_path(1).join("flicker.md");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !path_b.exists(),
        "File that existed only briefly should not leak to peer"
    );
}

/// Offline deletion (client killed, file removed, client restarted) must
/// propagate to the peer. Verifies `bootstrap_offline_changes` deletion path.
#[tokio::test]
async fn test_offline_delete_text_file_propagates() {
    let mut env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("offline_delete.md");
    fs::write(&path_a, "content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    env.clients[0].kill().await.unwrap();
    fs::remove_file(&path_a).unwrap();
    env.restart_client(0).await;

    let path_b = env.client_path(1).join("offline_delete.md");
    let ok = wait_until(Duration::from_secs(15), || !path_b.exists()).await;
    assert!(ok, "Offline-deleted file should not exist on peer");
}

/// Offline deletion of a binary file. Known gap: the bootstrap's blob_hash
/// clearing path exists but the peer's empty-blob-hash handler should delete
/// the file — verifies end-to-end behavior.
#[tokio::test]
async fn test_offline_delete_binary_file_propagates() {
    let mut env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("offline_bin.png");
    fs::write(&path_a, b"\x89PNG\r\n\x1a\n\x42\x42\x42").unwrap();
    let path_b = env.client_path(1).join("offline_bin.png");
    assert!(
        wait_until(Duration::from_secs(15), || path_b.exists()).await,
        "Binary should sync before delete"
    );

    env.clients[0].kill().await.unwrap();
    fs::remove_file(&path_a).unwrap();
    env.restart_client(0).await;

    let ok = wait_until(Duration::from_secs(15), || !path_b.exists()).await;
    assert!(ok, "Offline-deleted binary should not exist on peer");
}

// ---------------------------------------------------------------------------
// Rename
// ---------------------------------------------------------------------------

/// Rename of an empty file: content-based rename detection will mismatch
/// (both empty → ambiguous) so this is expected to fail under the current
/// heuristic.
#[tokio::test]
async fn test_rename_empty_file_preserves_identity() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("empty_orig.md");
    fs::write(&path_a, "").unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || env
            .client_path(1)
            .join("empty_orig.md")
            .exists())
        .await,
        "Empty file should sync first"
    );

    let renamed_a = env.client_path(0).join("empty_new.md");
    fs::rename(&path_a, &renamed_a).unwrap();

    let renamed_b = env.client_path(1).join("empty_new.md");
    let original_b = env.client_path(1).join("empty_orig.md");
    let ok = wait_until(Duration::from_secs(10), || {
        renamed_b.exists() && !original_b.exists()
    })
    .await;
    assert!(
        ok,
        "Rename of empty file should propagate (known gap: content-hash rename detection fails on empty files)"
    );
}

/// Rename of an unchanged binary file. Content-hash matching (by blob hash)
/// should correctly identify the rename rather than treat as delete+create.
#[tokio::test]
async fn test_rename_binary_file_preserves_identity() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("img_orig.png");
    let data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xCA, 0xFE];
    fs::write(&path_a, &data).unwrap();
    assert!(
        wait_until(Duration::from_secs(15), || env
            .client_path(1)
            .join("img_orig.png")
            .exists())
        .await,
        "Binary should sync first"
    );

    let renamed_a = env.client_path(0).join("img_renamed.png");
    fs::rename(&path_a, &renamed_a).unwrap();

    let renamed_b = env.client_path(1).join("img_renamed.png");
    let original_b = env.client_path(1).join("img_orig.png");
    let ok = wait_until(Duration::from_secs(15), || {
        renamed_b.exists() && !original_b.exists()
    })
    .await;
    assert!(ok, "Binary rename should propagate");
}

/// Rename to overwrite existing file (A → B where B already exists): should
/// destroy B on peer.
#[tokio::test]
async fn test_rename_overwrite_existing_file() {
    let env = TestEnv::new(2).await;

    let a = env.client_path(0).join("source.md");
    let b = env.client_path(0).join("target.md");
    fs::write(&a, "SOURCE").unwrap();
    fs::write(&b, "TARGET").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // rename(a, b) — on Unix this atomically replaces b.
    fs::rename(&a, &b).unwrap();

    let b_peer = env.client_path(1).join("target.md");
    let a_peer = env.client_path(1).join("source.md");
    let ok = wait_until(Duration::from_secs(10), || {
        !a_peer.exists()
            && fs::read_to_string(&b_peer)
                .map(|c| c == "SOURCE")
                .unwrap_or(false)
    })
    .await;
    assert!(
        ok,
        "Overwriting rename should end with source.md gone and target.md containing SOURCE"
    );
}

/// Rename across case difference only (`file.md` → `File.md`). On
/// case-insensitive filesystems (macOS default HFS+/APFS) this is a no-op at
/// the FS level; on Linux it's a real rename.
#[tokio::test]
async fn test_rename_case_change_only() {
    let env = TestEnv::new(2).await;

    let a = env.client_path(0).join("casefile.md");
    fs::write(&a, "content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    let a_up = env.client_path(0).join("CaseFile.md");
    // On case-insensitive FS this may succeed as a no-op rename.
    let _ = fs::rename(&a, &a_up);

    tokio::time::sleep(Duration::from_secs(3)).await;
    // We only verify sync does not crash and the file is still present under
    // *some* name — the canonical name depends on FS case-sensitivity.
    let peer_dir = env.client_path(1);
    let peer_files = common::collect_user_files(peer_dir);
    assert!(
        peer_files.iter().any(|f| f.eq_ignore_ascii_case("casefile.md")),
        "File should still be synced under some case on peer, got: {:?}",
        peer_files
    );
}

// ---------------------------------------------------------------------------
// Move (rename across directories)
// ---------------------------------------------------------------------------

/// Moving a file to a different directory should update `meta.path`, not
/// create a duplicate. The existing rename detector uses content-hash match,
/// which should work here since it ignores directory part.
#[tokio::test]
async fn test_move_file_across_directories() {
    let env = TestEnv::new(2).await;

    let src_dir = env.client_path(0).join("src");
    let dst_dir = env.client_path(0).join("dst");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();

    let src = src_dir.join("mover.md");
    fs::write(&src, "moving across dirs").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    let dst = dst_dir.join("mover.md");
    fs::rename(&src, &dst).unwrap();

    let peer_dst = env.client_path(1).join("dst/mover.md");
    let peer_src = env.client_path(1).join("src/mover.md");
    let ok = wait_until(Duration::from_secs(10), || {
        peer_dst.exists() && !peer_src.exists()
    })
    .await;
    assert!(ok, "Cross-directory move should propagate as rename");
}

/// Same as above, but for a binary file.
#[tokio::test]
async fn test_move_binary_across_directories() {
    let env = TestEnv::new(2).await;

    let src_dir = env.client_path(0).join("pictures");
    let dst_dir = env.client_path(0).join("archive");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();

    let src = src_dir.join("photo.png");
    fs::write(&src, [0x89u8, 0x50, 0x4E, 0x47, 0x12, 0x34].as_slice()).unwrap();
    assert!(
        wait_until(Duration::from_secs(15), || env
            .client_path(1)
            .join("pictures/photo.png")
            .exists())
        .await
    );

    let dst = dst_dir.join("photo.png");
    fs::rename(&src, &dst).unwrap();

    let peer_dst = env.client_path(1).join("archive/photo.png");
    let peer_src = env.client_path(1).join("pictures/photo.png");
    let ok = wait_until(Duration::from_secs(15), || {
        peer_dst.exists() && !peer_src.exists()
    })
    .await;
    assert!(ok, "Cross-directory binary move should propagate");
}
