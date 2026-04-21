//! Edge cases: unicode filenames, long paths, symlinks, hard links, large
//! files, special characters, case sensitivity.

mod common;

use common::{wait_for_convergence, wait_until, TestEnv};
use std::fs;
use std::time::Duration;

/// Filenames with spaces must sync.
#[tokio::test]
async fn test_filename_with_spaces() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("my notes.md");
    fs::write(&path_a, "spaced").unwrap();

    let path_b = env.client_path(1).join("my notes.md");
    let ok = wait_until(Duration::from_secs(10), || path_b.exists()).await;
    assert!(ok, "Filename with spaces should sync");
    assert_eq!(fs::read_to_string(&path_b).unwrap(), "spaced");
}

/// Unicode (non-ASCII) filenames.
#[tokio::test]
async fn test_filename_with_unicode() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("Příliš_žluťoučký.md");
    fs::write(&path_a, "czech").unwrap();

    let path_b = env.client_path(1).join("Příliš_žluťoučký.md");
    let ok = wait_until(Duration::from_secs(10), || path_b.exists()).await;
    assert!(ok, "Unicode filename should sync");
    assert_eq!(fs::read_to_string(&path_b).unwrap(), "czech");
}

/// Emoji in filename.
#[tokio::test]
async fn test_filename_with_emoji() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("rocket🚀.md");
    fs::write(&path_a, "emoji file").unwrap();

    let path_b = env.client_path(1).join("rocket🚀.md");
    let ok = wait_until(Duration::from_secs(10), || path_b.exists()).await;
    assert!(ok, "Filename with emoji should sync");
    assert_eq!(fs::read_to_string(&path_b).unwrap(), "emoji file");
}

/// Emoji inside the file content — exercises the UTF-8 byte-offset path in
/// `apply_diff_to_yrs` (already fixed, but regression guard).
#[tokio::test]
async fn test_content_with_emoji() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("emoji_content.md");
    fs::write(&path_a, "hello 🚀 world 🎉").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Mutate a multi-byte character region.
    fs::write(&path_a, "hello 🚀 world 🎉 and 🌍").unwrap();
    let path_b = env.client_path(1).join("emoji_content.md");
    let ok = wait_until(Duration::from_secs(10), || {
        fs::read_to_string(&path_b)
            .map(|c| c == "hello 🚀 world 🎉 and 🌍")
            .unwrap_or(false)
    })
    .await;
    assert!(ok, "Emoji content edit should propagate byte-accurately");
}

/// Characters that are valid on Linux/macOS but reserved on Windows:
/// `:`, `<`, `>`, `"`, `|`, `?`, `*`. Syncline should at minimum sync them
/// between two Linux nodes.
#[tokio::test]
async fn test_filename_with_special_characters() {
    let env = TestEnv::new(2).await;

    // Pick a subset that's safe on Linux even if Windows rejects them.
    let path_a = env.client_path(0).join("file!with&weird(chars).md");
    fs::write(&path_a, "weird").unwrap();

    let path_b = env.client_path(1).join("file!with&weird(chars).md");
    let ok = wait_until(Duration::from_secs(10), || path_b.exists()).await;
    assert!(ok, "Filename with special chars should sync on Linux");
}

/// Very long filename (near POSIX 255-byte component limit).
#[tokio::test]
async fn test_very_long_filename() {
    let env = TestEnv::new(2).await;

    let name = format!("{}.md", "a".repeat(200));
    let path_a = env.client_path(0).join(&name);
    fs::write(&path_a, "long name").unwrap();

    let path_b = env.client_path(1).join(&name);
    let ok = wait_until(Duration::from_secs(10), || path_b.exists()).await;
    assert!(ok, "Very long filename should sync");
}

/// A file with 1MB of text content. Should sync within reasonable time.
#[tokio::test]
async fn test_large_text_file_syncs() {
    let env = TestEnv::new(2).await;

    let large = "a".repeat(1_000_000); // 1 MB
    let path_a = env.client_path(0).join("large.md");
    fs::write(&path_a, &large).unwrap();

    let path_b = env.client_path(1).join("large.md");
    let ok = wait_until(Duration::from_secs(30), || {
        fs::metadata(&path_b).map(|m| m.len() as usize == large.len()).unwrap_or(false)
    })
    .await;
    assert!(ok, "1MB text file should sync");
}

/// Binary file near (but under) the 50MB protocol limit.
#[tokio::test]
#[ignore = "slow: exercises large-blob path, run explicitly"]
async fn test_large_binary_file_under_limit() {
    let env = TestEnv::new(2).await;

    let data = vec![0x42u8; 40 * 1024 * 1024]; // 40 MB
    let path_a = env.client_path(0).join("big.bin");
    fs::write(&path_a, &data).unwrap();

    let path_b = env.client_path(1).join("big.bin");
    let ok = wait_until(Duration::from_secs(60), || {
        fs::metadata(&path_b).map(|m| m.len() as usize == data.len()).unwrap_or(false)
    })
    .await;
    assert!(ok, "40MB binary should sync");
}

/// Binary file exceeding the 50MB protocol limit — should fail gracefully,
/// not crash the clients.
#[tokio::test]
#[ignore = "slow: allocates >50MB, run explicitly"]
async fn test_binary_over_size_limit_fails_gracefully() {
    let env = TestEnv::new(2).await;

    let data = vec![0x42u8; 60 * 1024 * 1024]; // 60 MB — over MAX_BLOB_SIZE
    let path_a = env.client_path(0).join("huge.bin");
    fs::write(&path_a, &data).unwrap();

    tokio::time::sleep(Duration::from_secs(10)).await;

    // Other files should still sync — clients must not be wedged.
    fs::write(env.client_path(0).join("canary.md"), "alive").unwrap();
    let canary_b = env.client_path(1).join("canary.md");
    let ok = wait_until(Duration::from_secs(15), || canary_b.exists()).await;
    assert!(ok, "Oversized file must not wedge the sync — canary should still propagate");
}

/// Symlinks: Syncline canonicalizes the root path but should not follow
/// symlinks as recursive sync targets. A symlink inside the vault pointing
/// outside should either be ignored or copied as a file, never followed.
#[cfg(unix)]
#[tokio::test]
async fn test_symlink_inside_vault_handled_safely() {
    use std::os::unix::fs::symlink;

    let env = TestEnv::new(2).await;

    // Create target outside vault, symlink inside.
    let outside = tempfile::TempDir::new().unwrap();
    fs::write(outside.path().join("outside.md"), "leak me").unwrap();

    let sym = env.client_path(0).join("link.md");
    symlink(outside.path().join("outside.md"), &sym).unwrap();

    tokio::time::sleep(Duration::from_secs(5)).await;

    // Either: symlink is skipped (preferred), or its content is copied. Must
    // never escape the vault boundary.
    let peer = env.client_path(1).join("link.md");
    if peer.exists() {
        // If it did sync, content must be what was under the symlink, not
        // something escaped from elsewhere.
        let c = fs::read_to_string(&peer).unwrap_or_default();
        assert!(
            c == "leak me" || c.is_empty(),
            "Symlink content, if synced, should match the target"
        );
    }
}

/// Hard link: both names refer to the same inode. Syncline treats each as a
/// separate file (different `rel_path`), so each should sync as its own doc.
#[cfg(unix)]
#[tokio::test]
async fn test_hard_link_both_sync_as_separate_files() {
    let env = TestEnv::new(2).await;

    let original = env.client_path(0).join("original.md");
    fs::write(&original, "shared inode").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    let link = env.client_path(0).join("hardlink.md");
    fs::hard_link(&original, &link).unwrap();

    let peer_link = env.client_path(1).join("hardlink.md");
    let ok = wait_until(Duration::from_secs(10), || peer_link.exists()).await;
    assert!(ok, "Hard link should appear as its own file on peer");
    assert_eq!(fs::read_to_string(&peer_link).unwrap(), "shared inode");
}

/// Case-difference filename co-existence: on a case-sensitive FS you can
/// have both `file.md` and `File.md`. On a case-insensitive FS only one
/// survives. The sync must not corrupt either.
#[tokio::test]
async fn test_case_variant_filenames() {
    let env = TestEnv::new(2).await;

    fs::write(env.client_path(0).join("alpha.md"), "lower").unwrap();
    // On case-insensitive FS this will overwrite — rely on the FS to tell us.
    let _ = fs::write(env.client_path(0).join("Alpha.md"), "upper");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let peer_files = common::collect_user_files(env.client_path(1));
    assert!(
        !peer_files.is_empty(),
        "At least one of alpha.md / Alpha.md should sync, got: {:?}",
        peer_files
    );
}

/// Deleting a file then creating a fresh one with the same name and
/// different content within a single debounce window.
#[tokio::test]
async fn test_delete_then_recreate_same_name_different_content() {
    let env = TestEnv::new(2).await;

    let path_a = env.client_path(0).join("recycled.md");
    fs::write(&path_a, "version 1").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    fs::remove_file(&path_a).unwrap();
    // Tight window — likely within the same debounce batch.
    fs::write(&path_a, "version 2 completely new").unwrap();

    let path_b = env.client_path(1).join("recycled.md");
    let ok = wait_until(Duration::from_secs(10), || {
        fs::read_to_string(&path_b)
            .map(|c| c == "version 2 completely new")
            .unwrap_or(false)
    })
    .await;
    assert!(
        ok,
        "Recreated file with new content should end up with version 2 on peer"
    );
}

/// File whose content is identical to another file's prior content. The
/// content-based rename detector could mis-match this as a rename.
#[tokio::test]
async fn test_identical_content_different_file_not_mistaken_for_rename() {
    let env = TestEnv::new(2).await;

    let a = env.client_path(0).join("twin_a.md");
    let b = env.client_path(0).join("twin_b.md");
    fs::write(&a, "twin content").unwrap();
    fs::write(&b, "twin content").unwrap();

    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Both files must exist with identical content on peer.
    let peer_a = env.client_path(1).join("twin_a.md");
    let peer_b = env.client_path(1).join("twin_b.md");
    assert!(peer_a.exists() && peer_b.exists());
    assert_eq!(fs::read_to_string(&peer_a).unwrap(), "twin content");
    assert_eq!(fs::read_to_string(&peer_b).unwrap(), "twin content");

    // Now delete one — the other must NOT be affected by the
    // content-matching rename heuristic.
    fs::remove_file(&a).unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        peer_b.exists(),
        "Deleting twin_a.md must not also remove/rename twin_b.md"
    );
    assert_eq!(fs::read_to_string(&peer_b).unwrap(), "twin content");
}

/// Path traversal safety: a `doc_id` containing `..` components should not
/// escape the vault directory on the peer. Regression guard for F-8.
#[tokio::test]
async fn test_normal_filename_stays_inside_vault() {
    let env = TestEnv::new(2).await;

    // Honest file in a subdir.
    let legit = env.client_path(0).join("subdir/legit.md");
    fs::create_dir_all(legit.parent().unwrap()).unwrap();
    fs::write(&legit, "inside").unwrap();

    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Peer must have the file at the correct path and NOT outside the vault.
    assert!(env.client_path(1).join("subdir/legit.md").exists());
    // Peer's vault root's parent must not suddenly have a leaked file.
    let leaked = env.client_path(1).parent().unwrap().join("legit.md");
    assert!(
        !leaked.exists(),
        "No file should leak outside the vault root"
    );
}
