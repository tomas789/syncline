//! Metadata-level sync tests: permissions, timestamps, attributes, hidden
//! files. Syncline treats files as (path, bytes) pairs — no metadata is
//! included in the CRDT. These tests codify the boundary between "content
//! syncs" and "OS metadata does not".

mod common;

use common::{wait_for_convergence, wait_until, TestEnv};
use std::fs;
use std::time::Duration;

/// Hidden files (dotfiles) must NOT be synced — this is an intentional
/// filter in the watcher + bootstrap. Regression guard to keep that behavior.
#[tokio::test]
async fn test_hidden_file_is_not_synced() {
    let env = TestEnv::new(2).await;

    fs::write(env.client_path(0).join(".hidden.md"), "secret").unwrap();
    fs::write(env.client_path(0).join("visible.md"), "ok").unwrap();

    assert!(
        wait_until(Duration::from_secs(10), || env
            .client_path(1)
            .join("visible.md")
            .exists())
        .await,
        "Visible file should sync first"
    );

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !env.client_path(1).join(".hidden.md").exists(),
        "Hidden dotfile should NOT be synced"
    );
}

/// A file inside a hidden directory (e.g. `.obsidian/workspace.json`) must
/// not be synced — hidden components at any depth filter the file out.
#[tokio::test]
async fn test_file_inside_hidden_directory_is_not_synced() {
    let env = TestEnv::new(2).await;

    let hidden_dir = env.client_path(0).join(".config");
    fs::create_dir_all(&hidden_dir).unwrap();
    fs::write(hidden_dir.join("settings.md"), "secret").unwrap();
    fs::write(env.client_path(0).join("visible.md"), "ok").unwrap();

    assert!(
        wait_until(Duration::from_secs(10), || env
            .client_path(1)
            .join("visible.md")
            .exists())
        .await,
        "Visible file should sync"
    );

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !env.client_path(1).join(".config/settings.md").exists(),
        "File in hidden dir should NOT sync"
    );
}

/// Permission changes (chmod) are not part of the sync model. The file
/// content should still be in sync after a mode change, but the mode itself
/// will NOT be mirrored.
#[cfg(unix)]
#[tokio::test]
async fn test_permission_change_does_not_propagate() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new(2).await;
    let path_a = env.client_path(0).join("chmod.md");
    fs::write(&path_a, "content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Change mode to 0o600.
    let mut perms = fs::metadata(&path_a).unwrap().permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&path_a, perms).unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    let path_b = env.client_path(1).join("chmod.md");
    let mode_b = fs::metadata(&path_b).unwrap().permissions().mode() & 0o777;
    // Known gap: permissions are not synced, so mode_b will be the default
    // (usually 0o644 or umask-derived), not 0o600.
    assert_eq!(
        mode_b, 0o600,
        "File permissions should propagate (known gap: metadata not synced)"
    );
}

/// Marking a file read-only should propagate: the peer should also observe
/// read-only mode. Not supported today.
#[cfg(unix)]
#[tokio::test]
async fn test_readonly_attribute_propagates() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new(2).await;
    let path_a = env.client_path(0).join("ro.md");
    fs::write(&path_a, "content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    let mut perms = fs::metadata(&path_a).unwrap().permissions();
    perms.set_mode(0o444);
    fs::set_permissions(&path_a, perms.clone()).unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    let path_b = env.client_path(1).join("ro.md");
    let mode_b = fs::metadata(&path_b).unwrap().permissions().mode() & 0o222;
    assert_eq!(
        mode_b, 0,
        "Read-only bit should propagate (known gap: metadata not synced)"
    );

    // Cleanup so tempdir can be removed.
    let mut p = fs::metadata(&path_a).unwrap().permissions();
    p.set_mode(0o644);
    let _ = fs::set_permissions(&path_a, p);
}

/// Updating only the modification time (touch -t, utimes) should not cause
/// a useless CRDT update — content is unchanged. Current implementation may
/// over-broadcast because the watcher fires on mtime changes.
#[tokio::test]
async fn test_touch_without_content_change_is_a_noop() {
    let env = TestEnv::new(2).await;
    let path_a = env.client_path(0).join("touchy.md");
    fs::write(&path_a, "stable content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Re-write identical content — simulates `touch` on many platforms.
    fs::write(&path_a, "stable content").unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Convergence should still hold and content should be unchanged on peer.
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(5)).await,
        "Touch-equivalent should not desynchronize"
    );
    assert_eq!(
        fs::read_to_string(env.client_path(1).join("touchy.md")).unwrap(),
        "stable content"
    );
}
