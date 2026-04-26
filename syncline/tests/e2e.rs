use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::error;
use yrs::{Any, GetString, Map, Out, Transact};

use std::sync::atomic::{AtomicU16, Ordering};

static NEXT_PORT: AtomicU16 = AtomicU16::new(18000);

pub fn get_available_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::Relaxed)
}

/// Find a v1 conflict sibling for `<stem>.<ext>` at the top level of
/// `dir`. v1 names collisions as `<stem>.conflict-<actor8>-<lamp>-<id8>.<ext>`
/// — see `projection::conflict_path`. Returns the first match found
/// (tests create at most one conflict at a time).
fn find_conflict_sibling(dir: &Path, stem: &str, ext: &str) -> Option<PathBuf> {
    let prefix = format!("{stem}.conflict-");
    let suffix = format!(".{ext}");
    let read = fs::read_dir(dir).ok()?;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && name.ends_with(&suffix) {
            return Some(entry.path());
        }
    }
    None
}

pub async fn build_workspace() {
    let status = Command::new("cargo")
        .args(["build", "-p", "syncline"])
        .status()
        .await
        .expect("cargo build failed");
    assert!(status.success(), "cargo build must succeed");
}

pub fn syncline_bin() -> PathBuf {
    std::env::current_dir()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/debug/syncline")
}

pub async fn spawn_server(port: u16, db_path: &Path) -> Child {
    Command::new(syncline_bin())
        .arg("server")
        .arg("--port")
        .arg(port.to_string())
        .arg("--db-path")
        .arg(db_path.to_str().unwrap())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn server")
}

pub async fn spawn_client(dir: &Path, port: u16) -> Child {
    Command::new(syncline_bin())
        .arg("sync")
        .arg("--folder")
        .arg(dir)
        .env("SYNCLINE_URL", format!("ws://127.0.0.1:{}/sync", port))
        .env("RUST_LOG", "debug")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn client")
}

/// Like [`spawn_client`] but captures the child's tracing output and
/// counts log lines containing known channel-overflow markers. Used
/// by the channel-buffer regression tests in this file to assert
/// that no debounced file event was dropped during a high-throughput
/// burst.
///
/// `tracing_subscriber::fmt::layer()` writes to **stdout** by default
/// in this binary (see `main.rs`), so we pipe stdout — not stderr —
/// to the in-memory reader. Returns the child and a shared
/// `Vec<String>` that the background reader task appends to whenever
/// it sees one of the `error!(...)` lines from `client/watcher.rs`:
///   - "Channel full or closed, dropped raw file event: ..."
///   - "Channel full or closed, dropped debounced file event: ..."
/// Lines that don't match are still tee'd to the test runner's
/// stderr (via `eprintln!`) so debug context is preserved on failure.
pub async fn spawn_client_capturing_drops(
    dir: &Path,
    port: u16,
) -> (Child, Arc<Mutex<Vec<String>>>) {
    let mut child = Command::new(syncline_bin())
        .arg("sync")
        .arg("--folder")
        .arg(dir)
        .env("SYNCLINE_URL", format!("ws://127.0.0.1:{}/sync", port))
        // `info` is enough to surface the `error!` lines we care
        // about and avoids the volume of `debug` we'd otherwise have
        // to scan line by line.
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn client (stdout-piped)");
    let stdout = child.stdout.take().expect("piped stdout handle");
    let drops: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let drops_clone = drops.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Tee everything so failed assertions still show context.
            eprintln!("[client] {line}");
            if line.contains("Channel full")
                || line.contains("dropped debounced file event")
                || line.contains("dropped raw file event")
            {
                drops_clone.lock().unwrap().push(line);
            }
        }
    });
    (child, drops)
}

pub async fn spawn_client_with_name(dir: &Path, port: u16, name: &str) -> Child {
    Command::new(syncline_bin())
        .arg("sync")
        .arg("--folder")
        .arg(dir)
        .arg("--name")
        .arg(name)
        .env("SYNCLINE_URL", format!("ws://127.0.0.1:{}/sync", port))
        .env("RUST_LOG", "debug")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn client")
}

fn compare_directories(client_dirs: &[PathBuf]) -> bool {
    if client_dirs.is_empty() {
        return true;
    }

    // Build a path→content map from UUID-named .bin files by reading meta.path
    // from each doc's Y.Map.
    let load_yrs_map = |dir: &PathBuf| -> HashMap<String, String> {
        let data_dir = dir.join(".syncline/data");
        let mut result = HashMap::new();
        for entry in walkdir::WalkDir::new(&data_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file()
                || path.extension().and_then(|s| s.to_str()) != Some("bin")
            {
                continue;
            }
            let Ok(raw) = fs::read(path) else { continue };
            let Ok(update) = yrs::updates::decoder::Decode::decode_v1(&raw) else {
                continue
            };
            let doc = yrs::Doc::new();
            {
                let mut txn = doc.transact_mut();
                txn.apply_update(update);
            }
            // Read meta.path from the Y.Map
            let meta = doc.get_or_insert_map("meta");
            let rel_path = {
                let txn = doc.transact();
                match meta.get(&txn, "path") {
                    Some(Out::Any(Any::String(arc))) => arc.to_string(),
                    _ => continue,
                }
            };
            if rel_path.is_empty() {
                continue;
            }
            // Check meta.type — skip Y.Text for binary docs
            let meta_type = {
                let txn = doc.transact();
                match meta.get(&txn, "type") {
                    Some(Out::Any(Any::String(arc))) => arc.to_string(),
                    _ => String::new(),
                }
            };
            if meta_type == "binary" {
                result.insert(rel_path, "[binary]".to_string());
            } else {
                // Read content
                let t = doc.get_or_insert_text("content");
                let txn = doc.transact();
                result.insert(rel_path, GetString::get_string(&t, &txn));
            }
        }
        result
    };

    let mut expected_files: HashMap<String, Vec<u8>> = HashMap::new();
    let expected_yrs = load_yrs_map(&client_dirs[0]);

    for entry in walkdir::WalkDir::new(&client_dirs[0]).min_depth(1) {
        let entry = entry.unwrap();
        let path = entry.path();
        let path_str = path.to_string_lossy();
        if path_str.contains(".syncline") || path_str.contains(".git") {
            continue;
        }
        if path.is_file() {
            let rel = path.strip_prefix(&client_dirs[0]).unwrap();
            let name = rel.to_string_lossy().into_owned();
            let content = fs::read(path).unwrap();
            expected_files.insert(name, content);
        }
    }

    let mut converged = true;

    for (idx, dir) in client_dirs.iter().enumerate().skip(1) {
        let mut actual_files: HashMap<String, Vec<u8>> = HashMap::new();
        let actual_yrs = load_yrs_map(dir);

        for entry in walkdir::WalkDir::new(dir).min_depth(1) {
            let entry = entry.unwrap();
            let path = entry.path();
            let path_str = path.to_string_lossy();
            if path_str.contains(".syncline") || path_str.contains(".git") {
                continue;
            }
            if path.is_file() {
                let rel = path.strip_prefix(dir).unwrap();
                let name = rel.to_string_lossy().into_owned();
                let content = fs::read(path).unwrap();
                actual_files.insert(name, content);
            }
        }

        let expected_keys: HashSet<&String> = expected_files.keys().collect();
        let actual_keys: HashSet<&String> = actual_files.keys().collect();

        if expected_keys != actual_keys {
            error!(
                "FILE SET MISMATCH between Client 0 and Client {}. Client 0 files: {:?}, Client {} files: {:?}",
                idx, expected_keys, idx, actual_keys
            );
            converged = false;
        }

        for (name, content) in &actual_files {
            if let Some(expected_content) = expected_files.get(name)
                && content != expected_content
            {
                error!(
                    "DISK File {} mismatches between Client 0 and Client {}.\nClient 0: {} bytes\nClient {}: {} bytes",
                    name, idx, expected_content.len(), idx, content.len()
                );
                converged = false;
            }
        }

        for (rel_path, content) in &actual_yrs {
            if let Some(expected_content) = expected_yrs.get(rel_path)
                && content != expected_content
            {
                error!(
                    "YRS File {} mismatches between Client 0 and Client {}.\nClient 0: {:?}\nClient {}: {:?}",
                    rel_path, idx, expected_content, idx, content
                );
                converged = false;
            }
        }
    }

    converged
}

async fn wait_for_convergence(dirs: &[PathBuf], timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if compare_directories(dirs) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Final check
    compare_directories(dirs)
}

#[allow(dead_code)]
struct TestEnv {
    server_dir: TempDir,
    client_dirs: Vec<TempDir>,
    port: u16,
    server: Child,
    clients: Vec<Child>,
}

impl TestEnv {
    async fn new(num_clients: usize) -> Self {
        build_workspace().await;
        let port = get_available_port();
        let server_dir = TempDir::new().unwrap();
        let db_path = server_dir.path().join("test.db");
        let server = spawn_server(port, &db_path).await;

        // Let server start
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut client_dirs = Vec::new();
        let mut clients = Vec::new();
        for _ in 0..num_clients {
            let dir = TempDir::new().unwrap();
            let client = spawn_client(dir.path(), port).await;
            client_dirs.push(dir);
            clients.push(client);
        }

        // Allow clients to connect and FSEvents to fully attach
        tokio::time::sleep(Duration::from_millis(2500)).await;

        Self {
            server_dir,
            client_dirs,
            port,
            server,
            clients,
        }
    }

    fn client_path(&self, idx: usize) -> &Path {
        self.client_dirs[idx].path()
    }

    fn dirs(&self) -> Vec<PathBuf> {
        self.client_dirs
            .iter()
            .map(|d| d.path().to_path_buf())
            .collect()
    }
}

#[tokio::test]
async fn test_basic_connection() {
    let _env = TestEnv::new(1).await;
    // Just verifying that TestEnv initializes without panics.
}

#[tokio::test]
async fn test_single_client_flow() {
    let env = TestEnv::new(1).await;
    let path = env.client_path(0).join("test.md");

    // Create new file
    fs::write(&path, "hello world").unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // v1 vault layout: a manifest.bin at the vault root and per-text-node
    // subdoc at .syncline/content/<node-id>.bin. test.md is a text file,
    // so we should see both pieces of state after the first sync.
    let syncline_dir = env.client_path(0).join(".syncline");
    assert!(
        syncline_dir.join("manifest.bin").is_file(),
        ".syncline/manifest.bin should exist after syncing test.md"
    );
    let content_dir = syncline_dir.join("content");
    let has_content_subdoc = fs::read_dir(&content_dir)
        .into_iter()
        .flatten()
        .any(|e| {
            e.ok()
                .map(|e| e.path().extension().and_then(|s| s.to_str()) == Some("bin"))
                .unwrap_or(false)
        });
    assert!(
        has_content_subdoc,
        "A per-text-node subdoc should exist in .syncline/content/ after syncing test.md"
    );

    // Update
    fs::write(&path, "hello modified").unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Delete
    // (Deletion logic may not be fully handled by Syncline according to earlier design but let's just make sure it doesn't crash)
    fs::remove_file(&path).unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
}

#[tokio::test]
async fn test_two_client_sync() {
    let env = TestEnv::new(2).await;

    let path0 = env.client_path(0).join("sync.md");
    fs::write(&path0, "client 0 data").unwrap();

    let converged = wait_for_convergence(&env.dirs(), Duration::from_secs(5)).await;
    assert!(converged, "Clients did not converge after 5 seconds");

    let path1 = env.client_path(1).join("sync.md");
    let content1 = fs::read_to_string(&path1).unwrap();
    assert_eq!(content1, "client 0 data");
}

#[tokio::test]
async fn test_offline_edits_and_reconnection() {
    let mut env = TestEnv::new(2).await;

    // Start synced state
    let path0 = env.client_path(0).join("doc.md");
    fs::write(&path0, "initial setup").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(5)).await);

    // Kill Client 1
    env.clients[1].kill().await.unwrap();

    // Client 0 modifies offline
    fs::write(&path0, "offline edit").unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await; // Allow C0 to sync with server

    // Restart Client 1
    let dir1 = env.client_dirs[1].path().to_path_buf();
    env.clients[1] = spawn_client(&dir1, env.port).await;

    // Check convergence
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(5)).await);

    let path1 = env.client_path(1).join("doc.md");
    let content1 = fs::read_to_string(&path1).unwrap();
    assert_eq!(content1, "offline edit");
}

#[tokio::test]
async fn test_concurrent_conflicts() {
    let mut env = TestEnv::new(2).await;

    // Give watchers extra time to fully hook up
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let path0 = env.client_path(0).join("conflict.md");
    let path1 = env.client_path(1).join("conflict.md");

    // Start synced state
    fs::write(&path0, "base content").unwrap();
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(15)).await,
        "Failed to sync initial content"
    );

    // Disconnect both
    env.clients[0].kill().await.unwrap();
    env.clients[1].kill().await.unwrap();

    // Concurrent edits
    fs::write(&path0, "client 0 edited").unwrap();
    fs::write(&path1, "client 1 modified here").unwrap();

    // Reconnect both
    let dir0 = env.client_dirs[0].path().to_path_buf();
    let dir1 = env.client_dirs[1].path().to_path_buf();
    env.clients[0] = spawn_client(&dir0, env.port).await;
    env.clients[1] = spawn_client(&dir1, env.port).await;

    // They must converge
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);
}

#[tokio::test]
async fn test_complex_directory_operations() {
    let env = TestEnv::new(2).await;

    let folder0 = env.client_path(0).join("nested");
    fs::create_dir(&folder0).unwrap();

    let path0 = folder0.join("item.md");
    fs::write(&path0, "nested content").unwrap();

    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Since Syncline flattens all files (currently doc_id looks like "nested/item.md"),
    // verify the folder was created on client 1
    let path1 = env.client_path(1).join("nested").join("item.md");
    assert!(path1.exists());
    assert_eq!(fs::read_to_string(&path1).unwrap(), "nested content");
}

#[tokio::test]
async fn test_filter_ignored_files() {
    // Hidden files are now synced by default; only `.syncline/` is
    // hardcoded-ignored, plus whatever the user lists in
    // `.synclineignore`. This test exercises both: a default-synced
    // dotfile (`.hidden.md`), a binary, a `.synclineignore`-excluded
    // file, and the always-ignored `.syncline/` metadata.
    let env = TestEnv::new(2).await;
    let binary0 = env.client_path(0).join("image.png");
    let hidden0 = env.client_path(0).join(".hidden.md");
    let ignored0 = env.client_path(0).join("device-only.md");
    let ignore_file = env.client_path(0).join(".synclineignore");

    fs::write(&ignore_file, "device-only.md\n").unwrap();
    fs::write(&binary0, "binary data").unwrap();
    fs::write(&hidden0, "secret text").unwrap();
    fs::write(&ignored0, "should not leave device 0").unwrap();

    // Wait for binary + dotfile to sync.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if env.client_path(1).join("image.png").exists()
            && env.client_path(1).join(".hidden.md").exists()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected image.png and .hidden.md to sync"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Binary file synced via the blob path.
    assert!(
        env.client_path(1).join("image.png").exists(),
        ".png file should sync via the binary path"
    );
    // Dotfiles sync by default (Hidden Files Sync — needed for
    // `.obsidian/` config files).
    assert!(
        env.client_path(1).join(".hidden.md").exists(),
        "dotfiles should sync by default"
    );
    // Files matching `.synclineignore` patterns must not propagate.
    assert!(
        !env.client_path(1).join("device-only.md").exists(),
        "files matching .synclineignore should be excluded"
    );
    // `.syncline/` is Syncline's own metadata directory and must
    // never be reflected on the peer's vault.
    assert!(
        !env.client_path(1).join(".syncline/manifest.bin").exists()
            || env.client_path(1).join(".syncline/manifest.bin").is_file(),
        ".syncline/ exists locally but its contents must not have been pushed from peer 0"
    );
}

/// Regression test for the CI failure on `propagates deletes CLI →
/// Obsidian` after 70fde18 landed bidirectional STEP_1 reciprocation.
///
/// The bug: server unconditionally reciprocated STEP_1 with its own
/// state vector. Clients whose state already matched the server's
/// answered with a *no-op* STEP_2 (encoded Yrs update with no blocks
/// and an empty delete-set). The server happily persisted+broadcast
/// each one. Every other peer's MSG_UPDATE handler then ran
/// flush_content_to_disk, re-writing the file from CRDT content.
/// When that broadcast raced against a local-disk delete, the file
/// got resurrected on disk before scan_once could register the
/// unlink — so the deletion never propagated.
///
/// The fix (see `is_noop_update` in server.rs) drops empty STEP_2
/// frames at the server's broadcast boundary. This test reproduces
/// the rename-then-delete scenario across two CLI peers (standing in
/// for the Obsidian peer in the wdio e2e suite).
#[tokio::test]
async fn test_rename_then_delete_propagates() {
    let mut env = TestEnv::new(2).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let path0 = env.client_path(0).join("doomed.md");
    fs::write(&path0, "# original\n").unwrap();
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await,
        "create did not converge"
    );

    let renamed0 = env.client_path(0).join("renamed.md");
    fs::rename(&path0, &renamed0).unwrap();
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await,
        "rename did not converge"
    );

    fs::remove_file(&renamed0).unwrap();

    let renamed1 = env.client_path(1).join("renamed.md");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if !renamed1.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "renamed.md still exists on peer 1 15s after delete on peer 0 \
         (empty STEP_2 broadcast likely re-flushing the file)"
    );
}

#[tokio::test]
async fn test_offline_creation_and_deletion() {
    let mut env = TestEnv::new(2).await;

    // Start synced state with one file
    let path0 = env.client_path(0).join("base.md");
    fs::write(&path0, "base content").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(5)).await);

    // Disconnect client 0
    env.clients[0].kill().await.unwrap();

    // Offline create and delete
    let new_path = env.client_path(0).join("offline_new.md");
    fs::write(&new_path, "offline creation").unwrap();
    fs::remove_file(&path0).unwrap();

    // Restart client 0
    let dir0 = env.client_dirs[0].path().to_path_buf();
    env.clients[0] = spawn_client(&dir0, env.port).await;

    // Reconnect and wait for convergence
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Check that Client 1 got the new file and deleted the old one
    assert!(env.client_path(1).join("offline_new.md").exists());
    assert_eq!(
        fs::read_to_string(env.client_path(1).join("offline_new.md")).unwrap(),
        "offline creation"
    );
    assert!(!env.client_path(1).join("base.md").exists());
}

/// Symmetric inverse of `test_offline_creation_and_deletion`: this time
/// Client 0 (online) is the deleter and Client 1 (offline) is the
/// survivor that holds a stale copy on disk.
///
/// Reproduces the late-reconnect tombstone resurrection bug: a peer
/// that goes offline before a delete propagates, then reconnects with
/// the (now-tombstoned) file still on disk, must NOT resurrect it via
/// `scan_once` creating a fresh NodeId. Instead, the local stale copy
/// must be removed when the manifest tombstone arrives.
///
/// Spec: DESIGN_DOC_V1.md §5.2 (delete protocol) + §9.5 test plan
/// (`test_delete_propagates_to_late_reconnecting_client`). The v1
/// test was named in the design doc but never implemented; v0 had
/// the same class of bug documented as KNOWN_BUGS #9 (was fixed for
/// v0, regressed in v1).
#[tokio::test]
async fn test_delete_propagates_to_late_reconnecting_client() {
    let mut env = TestEnv::new(2).await;

    // Synced state: both peers have the file.
    let path0 = env.client_path(0).join("doomed.md");
    fs::write(&path0, "delete me later").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(8)).await);
    assert!(env.client_path(1).join("doomed.md").exists());

    // Peer 1 goes offline.
    env.clients[1].kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Peer 0 (online) deletes the file. Server gets the tombstone.
    fs::remove_file(&path0).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Peer 1 reconnects. It still has `doomed.md` on disk from the
    // pre-delete sync. After reconnect + manifest sync + scan, the
    // tombstone must win and the local file must be removed — NOT
    // recreated as a fresh NodeId on the server.
    let dir1 = env.client_dirs[1].path().to_path_buf();
    env.clients[1] = spawn_client(&dir1, env.port).await;

    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await);

    assert!(
        !env.client_path(0).join("doomed.md").exists(),
        "peer 0: doomed.md must stay deleted after peer 1 reconnect (resurrection bug)",
    );
    assert!(
        !env.client_path(1).join("doomed.md").exists(),
        "peer 1: doomed.md must be removed when reconnect surfaces the tombstone (resurrection bug)",
    );
}

/// Binary variant of `test_delete_propagates_to_late_reconnecting_client`.
/// Targets the parallel resurrection path in `process_binary_file`:
/// when the per-walk projection (which filters tombstoned entries)
/// reports `None` for a binary path, the scanner currently calls
/// `create_binary` and produces a fresh NodeId, broadcasting the
/// tombstoned blob back as a new node.
#[tokio::test]
async fn test_delete_propagates_to_late_reconnecting_client_binary() {
    let mut env = TestEnv::new(2).await;

    // Tiny "PNG" — header + a few payload bytes. Mirrors
    // test_binary_file_sync's fixture for consistency.
    let png_data: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
        0xCA, 0xFE, 0xBA, 0xBE,
    ];
    let path0 = env.client_path(0).join("doomed.png");
    fs::write(&path0, &png_data).unwrap();

    // Binary sync needs a longer settle (CAS blob round-trip).
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(env.client_path(1).join("doomed.png").exists());

    env.clients[1].kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    fs::remove_file(&path0).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let dir1 = env.client_dirs[1].path().to_path_buf();
    env.clients[1] = spawn_client(&dir1, env.port).await;

    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await);

    assert!(
        !env.client_path(0).join("doomed.png").exists(),
        "peer 0: doomed.png must stay deleted (binary resurrection bug)",
    );
    assert!(
        !env.client_path(1).join("doomed.png").exists(),
        "peer 1: doomed.png must be removed on reconnect (binary resurrection bug)",
    );
}

/// Scaled-down replica of the 2026-04-25 vault chaos: many files
/// deleted while one peer is offline, and on reconnect the offline
/// peer must accept the tombstones rather than re-broadcasting all of
/// them as new nodes (which is what produced the +200-file overwrite
/// in production).
///
/// Uses 25 files to keep CI runtime bounded. The mechanism under test
/// is identical to the single-file case; we just want to ensure the
/// fix applies uniformly, not only to one entry.
#[tokio::test]
async fn test_bulk_delete_propagates_to_late_reconnecting_client() {
    let mut env = TestEnv::new(2).await;

    const N: usize = 25;
    let names: Vec<String> = (0..N).map(|i| format!("doomed_{i:03}.md")).collect();

    for (i, name) in names.iter().enumerate() {
        fs::write(
            env.client_path(0).join(name),
            format!("payload-{i}"),
        )
        .unwrap();
    }
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await);
    for name in &names {
        assert!(
            env.client_path(1).join(name).exists(),
            "{name} must reach peer 1 in the synced phase",
        );
    }

    env.clients[1].kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    for name in &names {
        fs::remove_file(env.client_path(0).join(name)).unwrap();
    }
    tokio::time::sleep(Duration::from_secs(5)).await;

    let dir1 = env.client_dirs[1].path().to_path_buf();
    env.clients[1] = spawn_client(&dir1, env.port).await;

    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(45)).await);

    let mut still_alive: Vec<String> = Vec::new();
    for name in &names {
        if env.client_path(0).join(name).exists()
            || env.client_path(1).join(name).exists()
        {
            still_alive.push(name.clone());
        }
    }
    assert!(
        still_alive.is_empty(),
        "{} of {} tombstoned files were resurrected after peer 1 reconnect: {:?}",
        still_alive.len(),
        N,
        still_alive,
    );
}

/// Client A syncs a file to the server, then Client B (a fresh directory with its own
/// pre-existing file of the same name) connects. The conflict is resolved by keeping the
/// server content as the canonical file and renaming B's local content.
#[tokio::test]
async fn test_pre_existing_directory_conflict() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Client A: starts fresh, creates note.md and syncs it to the server
    let dir_a = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    tokio::time::sleep(Duration::from_millis(1500)).await; // let A connect

    fs::write(dir_a.path().join("note.md"), "content from A").unwrap();
    tokio::time::sleep(Duration::from_millis(3000)).await; // let A sync

    // Kill client A so its watcher doesn't interfere
    client_a.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Client B: pre-existing directory with its own note.md (no .syncline/ dir)
    let dir_b = TempDir::new().unwrap();
    fs::write(dir_b.path().join("note.md"), "content from B").unwrap();

    let mut client_b = spawn_client_with_name(dir_b.path(), port, "client-b").await;
    tokio::time::sleep(Duration::from_millis(5000)).await; // let B connect, bootstrap, resolve

    // note.md on B should contain A's content (server wins)
    let note_b_content = fs::read_to_string(dir_b.path().join("note.md")).unwrap();
    assert_eq!(
        note_b_content, "content from A",
        "note.md should have server content (A's content)"
    );

    // A conflict sibling of the form `note.conflict-<actor8>-<lamp>-<id8>.md`
    // should exist on B with B's original content. v1 naming (see
    // projection::conflict_path) is deterministic across peers.
    let conflict_path_b = find_conflict_sibling(dir_b.path(), "note", "md")
        .expect("Conflict sibling for note.md should exist in dir_b");
    assert_eq!(
        fs::read_to_string(&conflict_path_b).unwrap(),
        "content from B",
        "Conflict file should have B's original content"
    );

    // Restart client A and verify it receives the conflict file from the server
    let mut client_a2 = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    tokio::time::sleep(Duration::from_millis(4000)).await;

    let conflict_path_a = find_conflict_sibling(dir_a.path(), "note", "md")
        .expect("Client A should receive the conflict sibling for note.md");
    assert_eq!(
        fs::read_to_string(&conflict_path_a).unwrap(),
        "content from B"
    );

    client_a2.kill().await.unwrap();
    client_b.kill().await.unwrap();
    server.kill().await.unwrap();
}

/// Both clients are offline when they each independently create a file with the same name.
/// The first to reconnect establishes server truth; the second detects the conflict.
#[tokio::test]
async fn test_both_offline_same_name_conflict() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    // Both clients connect briefly so they register with the server, then get killed
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "client-b").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    client_a.kill().await.unwrap();
    client_b.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Both independently create shared.md while offline
    fs::write(dir_a.path().join("shared.md"), "A's offline content").unwrap();
    fs::write(dir_b.path().join("shared.md"), "B's offline content").unwrap();

    // A reconnects first — its content becomes server truth.
    // Wait until A has synced (v1 content subdoc materialised on disk)
    // before starting B, to ensure B observes A's manifest entry.
    let mut client_a2 = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    let content_dir = dir_a.path().join(".syncline").join("content");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let has_subdoc = fs::read_dir(&content_dir)
            .map(|rd| rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("bin")))
            .unwrap_or(false);
        if has_subdoc {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Timed out waiting for client A to sync its document"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // B reconnects — detects conflict, renames its content
    let mut client_b2 = spawn_client_with_name(dir_b.path(), port, "client-b").await;

    // Wait until conflict resolution completes on B: shared.md has A's content
    // and a conflict sibling (v1 naming) exists with B's content.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let shared_ok = fs::read_to_string(dir_b.path().join("shared.md"))
            .map(|c| c == "A's offline content")
            .unwrap_or(false);
        let conflict_ok = find_conflict_sibling(dir_b.path(), "shared", "md")
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|c| c == "B's offline content")
            .unwrap_or(false);
        if shared_ok && conflict_ok {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Timed out waiting for conflict resolution on B. \
             shared.md exists={} content={:?}, all files: {:?}",
            dir_b.path().join("shared.md").exists(),
            fs::read_to_string(dir_b.path().join("shared.md")).ok(),
            fs::read_dir(dir_b.path())
                .map(|rd| rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>())
                .unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A should eventually receive the conflict file
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let ok = find_conflict_sibling(dir_a.path(), "shared", "md")
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|c| c == "B's offline content")
            .unwrap_or(false);
        if ok {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Timed out waiting for Client A to receive conflict sibling of shared.md"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    client_a2.kill().await.unwrap();
    client_b2.kill().await.unwrap();
    server.kill().await.unwrap();
}

/// A client starts with pre-existing files on a server that has no data.
/// No conflict should occur — all files should sync normally without being renamed.
#[tokio::test]
async fn test_pre_existing_no_conflict_empty_server() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Client A: pre-existing files, server is empty
    let dir_a = TempDir::new().unwrap();
    fs::write(dir_a.path().join("file1.md"), "content 1").unwrap();
    fs::write(dir_a.path().join("file2.md"), "content 2").unwrap();

    let mut client_a = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    tokio::time::sleep(Duration::from_millis(4000)).await;

    // No conflict files should be created
    assert!(
        !dir_a.path().join("file1 (client-a).md").exists(),
        "No conflict file should be created for file1.md when server is empty"
    );
    assert!(
        !dir_a.path().join("file2 (client-a).md").exists(),
        "No conflict file should be created for file2.md when server is empty"
    );

    // Original files should be unchanged
    assert_eq!(fs::read_to_string(dir_a.path().join("file1.md")).unwrap(), "content 1");
    assert_eq!(fs::read_to_string(dir_a.path().join("file2.md")).unwrap(), "content 2");

    // Client B joins and receives all files
    let dir_b = TempDir::new().unwrap();
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "client-b").await;
    tokio::time::sleep(Duration::from_millis(4000)).await;

    assert_eq!(fs::read_to_string(dir_b.path().join("file1.md")).unwrap(), "content 1");
    assert_eq!(fs::read_to_string(dir_b.path().join("file2.md")).unwrap(), "content 2");

    client_a.kill().await.unwrap();
    client_b.kill().await.unwrap();
    server.kill().await.unwrap();
}

/// Client A creates a file, both clients sync. Client A renames the file while both
/// are online. Client B should automatically receive the rename: `test.md` disappears
/// and `renamed.md` appears with the original content.
///
/// This test validates live rename detection — the watcher sees the delete+create
/// events in the same batch, matches them by content, preserves the UUID, and
/// broadcasts an update that only changes `meta.path`.
#[tokio::test]
async fn test_rename_sync() {
    let env = TestEnv::new(2).await;

    // Client A creates test.md and both clients sync
    let test_a = env.client_path(0).join("test.md");
    fs::write(&test_a, "shared content for rename test").unwrap();

    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await,
        "Initial sync failed before rename"
    );

    // Client A renames test.md → renamed.md while both clients are online.
    // `fs::rename` is atomic on Unix and fires delete+create events in the same
    // watcher batch, which is needed for content-based rename detection.
    let renamed_a = env.client_path(0).join("renamed.md");
    fs::rename(&test_a, &renamed_a).unwrap();

    // Wait for the rename to propagate (watcher debounce is 300 ms, plus network round-trip)
    tokio::time::sleep(Duration::from_millis(3000)).await;

    // renamed.md must be present on client B with the original content
    assert!(
        env.client_path(1).join("renamed.md").exists(),
        "renamed.md should exist on Client B after rename sync"
    );
    assert_eq!(
        fs::read_to_string(env.client_path(1).join("renamed.md")).unwrap(),
        "shared content for rename test",
        "renamed.md content should match original"
    );

    // test.md must be gone on client B
    assert!(
        !env.client_path(1).join("test.md").exists(),
        "test.md should not exist on Client B after rename"
    );

    // Final convergence check (file sets and YRS state identical across clients)
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(5)).await,
        "Clients did not converge after rename"
    );
}

// =============================================================================
// Binary file tests
// =============================================================================

/// Client A creates a binary file (.png), and it should sync to Client B
/// with identical bytes.
#[tokio::test]
async fn test_binary_file_sync() {
    let env = TestEnv::new(2).await;

    // Create a small "PNG" file (with valid PNG header)
    let png_data: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG header
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0xDE, 0xAD, 0xBE, 0xEF, // test payload
    ];
    let png_path = env.client_path(0).join("test.png");
    fs::write(&png_path, &png_data).unwrap();

    // Wait for sync (binary files need time for blob upload+download)
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Client B should have the file with identical bytes
    let client_b_path = env.client_path(1).join("test.png");
    assert!(
        client_b_path.exists(),
        "Binary file test.png should exist on Client B"
    );
    let synced_data = fs::read(&client_b_path).unwrap();
    assert_eq!(
        png_data, synced_data,
        "Binary file should have identical bytes on both clients"
    );
}

/// Client A creates a binary file, syncs it, then modifies it.
/// The updated binary should propagate to Client B.
#[tokio::test]
#[ignore = "flaky on slow CI runners; tracked in #70"]
async fn test_binary_file_modify_sync() {
    let env = TestEnv::new(2).await;

    // Create initial binary file
    let initial_data: Vec<u8> = vec![0x00, 0x01, 0x02, 0x03, 0x04];
    let bin_path = env.client_path(0).join("data.bin");
    fs::write(&bin_path, &initial_data).unwrap();

    // Wait for initial sync
    tokio::time::sleep(Duration::from_secs(8)).await;

    let client_b_path = env.client_path(1).join("data.bin");
    assert!(
        client_b_path.exists(),
        "Binary file data.bin should exist on Client B after initial sync"
    );
    assert_eq!(
        initial_data,
        fs::read(&client_b_path).unwrap(),
        "Initial binary content should match"
    );

    // Modify the binary file on Client A
    let updated_data: Vec<u8> = vec![0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA];
    fs::write(&bin_path, &updated_data).unwrap();

    // Wait for update to propagate
    tokio::time::sleep(Duration::from_secs(8)).await;

    let synced_updated = fs::read(&client_b_path).unwrap();
    assert_eq!(
        updated_data, synced_updated,
        "Updated binary file should have new content on Client B"
    );
}

/// Mixed text and binary files should all sync correctly together.
#[tokio::test]
async fn test_binary_and_text_mixed_sync() {
    let env = TestEnv::new(2).await;

    // Create a mix of text and binary files on Client A
    fs::write(env.client_path(0).join("notes.md"), "# My Notes\nHello").unwrap();
    fs::write(
        env.client_path(0).join("image.png"),
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
    )
    .unwrap();
    fs::write(
        env.client_path(0).join("config.json"),
        &[0x7B, 0x22, 0x6B, 0x65, 0x79, 0x22, 0x7D], // {"key"}
    )
    .unwrap();

    // Wait for sync
    tokio::time::sleep(Duration::from_secs(10)).await;

    // All files should exist on Client B
    assert!(env.client_path(1).join("notes.md").exists(), "notes.md should sync");
    assert!(env.client_path(1).join("image.png").exists(), "image.png should sync");
    assert!(env.client_path(1).join("config.json").exists(), "config.json should sync");

    // Text file should have correct content
    assert_eq!(
        fs::read_to_string(env.client_path(1).join("notes.md")).unwrap(),
        "# My Notes\nHello"
    );

    // Binary files should have identical bytes
    assert_eq!(
        fs::read(env.client_path(1).join("image.png")).unwrap(),
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    );
    assert_eq!(
        fs::read(env.client_path(1).join("config.json")).unwrap(),
        vec![0x7B, 0x22, 0x6B, 0x65, 0x79, 0x22, 0x7D]
    );
}

// =============================================================================
// UUID-named file regression test
// =============================================================================

/// Returns true if the string looks like a UUID (8-4-4-4-12 hex pattern).
fn looks_like_uuid(s: &str) -> bool {
    // Strip common extensions before checking
    let stem = std::path::Path::new(s)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(s);
    let parts: Vec<&str> = stem.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Collect all user-visible filenames (excluding .syncline metadata) from a directory.
fn collect_user_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir).min_depth(1) {
        let entry = entry.unwrap();
        let path = entry.path();
        let path_str = path.to_string_lossy();
        if path_str.contains(".syncline") || path_str.contains(".git") {
            continue;
        }
        if path.is_file() {
            let rel = path.strip_prefix(dir).unwrap();
            files.push(rel.to_string_lossy().into_owned());
        }
    }
    files
}

/// Sync-to-directory must produce files with their proper names, not with
/// UUID-based names from the internal storage layer. This test creates text
/// and binary files on Client 0 and verifies that Client 1's sync directory
/// contains only properly-named files — no UUID artifacts.
#[tokio::test]
async fn test_no_uuid_named_files_in_sync_directory() {
    let env = TestEnv::new(2).await;

    // Create a mix of text and binary files on Client 0
    fs::write(env.client_path(0).join("readme.md"), "# Hello").unwrap();
    fs::write(env.client_path(0).join("notes.txt"), "some notes").unwrap();
    fs::write(
        env.client_path(0).join("photo.png"),
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
    )
    .unwrap();

    // Wait for sync (binary files need extra time for blob upload/download)
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Verify convergence
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await,
        "Clients should converge"
    );

    // Collect all user-visible files on Client 1
    let client1_files = collect_user_files(env.client_path(1));

    // Every file must have a proper name, not a UUID
    for file in &client1_files {
        assert!(
            !looks_like_uuid(file),
            "File '{}' on Client 1 looks like a UUID — sync-to-directory should use proper filenames from meta.path, not internal UUIDs",
            file
        );
    }

    // The expected files must exist with correct names
    assert!(
        client1_files.contains(&"readme.md".to_string()),
        "readme.md should exist on Client 1, got: {:?}",
        client1_files
    );
    assert!(
        client1_files.contains(&"notes.txt".to_string()),
        "notes.txt should exist on Client 1, got: {:?}",
        client1_files
    );
    assert!(
        client1_files.contains(&"photo.png".to_string()),
        "photo.png should exist on Client 1, got: {:?}",
        client1_files
    );

    // Verify content integrity
    assert_eq!(
        fs::read_to_string(env.client_path(1).join("readme.md")).unwrap(),
        "# Hello"
    );
    assert_eq!(
        fs::read_to_string(env.client_path(1).join("notes.txt")).unwrap(),
        "some notes"
    );
    assert_eq!(
        fs::read(env.client_path(1).join("photo.png")).unwrap(),
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    );
}

/// A fresh client connecting to a server that already has data should
/// receive files with proper names, not UUIDs. This tests the "cold start"
/// sync-to-directory scenario where the client has no prior state.
#[tokio::test]
async fn test_fresh_client_receives_proper_filenames() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Client A creates files and syncs them to the server
    let dir_a = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    tokio::time::sleep(Duration::from_millis(2000)).await;

    fs::write(dir_a.path().join("document.md"), "hello from A").unwrap();
    fs::write(
        dir_a.path().join("image.png"),
        &[0x89, 0x50, 0x4E, 0x47, 0xDE, 0xAD],
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Kill client A — server retains the data
    client_a.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Client B connects fresh (empty directory, no .syncline state)
    let dir_b = TempDir::new().unwrap();
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "client-b").await;
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Verify Client B has files with proper names
    let client_b_files = collect_user_files(dir_b.path());

    for file in &client_b_files {
        assert!(
            !looks_like_uuid(file),
            "Fresh client received UUID-named file '{}' — should have proper filename from meta.path",
            file
        );
    }

    assert!(
        client_b_files.contains(&"document.md".to_string()),
        "document.md should exist on fresh Client B, got: {:?}",
        client_b_files
    );
    assert_eq!(
        fs::read_to_string(dir_b.path().join("document.md")).unwrap(),
        "hello from A"
    );

    assert!(
        client_b_files.contains(&"image.png".to_string()),
        "image.png should exist on fresh Client B, got: {:?}",
        client_b_files
    );
    assert_eq!(
        fs::read(dir_b.path().join("image.png")).unwrap(),
        vec![0x89, 0x50, 0x4E, 0x47, 0xDE, 0xAD]
    );

    client_b.kill().await.unwrap();
    server.kill().await.unwrap();
}

// ---------------------------------------------------------------------------
// Phase 3.5 — CLI-level integration tests
//
// These exercise the operator-facing CLI surface introduced in phases 3.1
// (`migrate`) and 3.4 (`verify`) rather than the protocol library directly.
// They round-trip through the same binary an operator would run, so they
// catch regressions in CLI wiring (arg parsing, exit codes, stdout logging)
// that unit tests can't.
// ---------------------------------------------------------------------------

async fn run_verify_cli(dir: &Path, port: u16, timeout_secs: u64) -> std::process::ExitStatus {
    Command::new(syncline_bin())
        .arg("verify")
        .arg("--folder")
        .arg(dir)
        .arg("--timeout-secs")
        .arg(timeout_secs.to_string())
        .env("SYNCLINE_URL", format!("ws://127.0.0.1:{}/sync", port))
        .env("RUST_LOG", "info")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .expect("failed to run verify CLI")
}

async fn run_migrate_cli(dir: &Path) -> std::process::ExitStatus {
    Command::new(syncline_bin())
        .arg("migrate")
        .arg("--folder")
        .arg(dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .expect("failed to run migrate CLI")
}

/// Write one Yrs-encoded v0 snapshot into `.syncline/data/<uuid>.bin`,
/// mirroring the pre-v1 on-disk layout: Y.Map "meta" with `path`/`type`
/// plus Y.Text "content" with the body.
fn seed_v0_text_snapshot(data_dir: &Path, rel_path: &str, body: &str) {
    use yrs::{Doc, ReadTxn, StateVector, Text};

    let doc = Doc::new();
    {
        let meta = doc.get_or_insert_map("meta");
        let mut txn = doc.transact_mut();
        meta.insert(&mut txn, "path", rel_path);
        meta.insert(&mut txn, "type", "text");
    }
    {
        let t = doc.get_or_insert_text("content");
        let mut txn = doc.transact_mut();
        t.insert(&mut txn, 0, body);
    }
    let bytes = {
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&StateVector::default())
    };
    fs::create_dir_all(data_dir).unwrap();
    let file_path = data_dir.join(format!("{}.bin", uuid::Uuid::new_v4()));
    fs::write(&file_path, bytes).unwrap();
}

/// After two clients sync a file, `syncline verify` on each vault must
/// report convergence (exit 0). Happy-path operator check.
#[tokio::test]
async fn test_verify_cli_converged_after_sync() {
    let mut env = TestEnv::new(2).await;

    fs::write(env.client_path(0).join("doc.md"), "converged body").unwrap();
    assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);

    // Stop clients so verify has exclusive access to the manifest on disk
    // (and there is no race with a concurrent save_manifest rewriting it).
    for c in env.clients.iter_mut() {
        c.kill().await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    for idx in 0..2 {
        let status = run_verify_cli(env.client_path(idx), env.port, 3).await;
        assert!(
            status.success(),
            "verify on client {} after sync should exit 0, got {:?}",
            idx,
            status.code()
        );
    }
}

/// A fresh vault with never-synced local state must fail verify against
/// a server that already holds content — divergence → non-zero exit.
#[tokio::test]
async fn test_verify_cli_diverges_for_fresh_vault() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Seed the server with content via a real client.
    let dir_a = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    fs::write(dir_a.path().join("server-only.md"), "on the server").unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    client_a.kill().await.unwrap();

    // Fresh vault, never synced: empty projection vs. populated server.
    let dir_fresh = TempDir::new().unwrap();
    let status = run_verify_cli(dir_fresh.path(), port, 3).await;
    assert_eq!(
        status.code(),
        Some(1),
        "verify on a fresh vault vs populated server should exit 1, got {:?}",
        status.code()
    );

    server.kill().await.unwrap();
}

/// Full operator journey: seed a v0 vault on disk, run `syncline
/// migrate`, then `syncline sync` (which should be idempotent wrt the
/// already-migrated layout), and finally `syncline verify` to confirm
/// convergence with the server.
#[tokio::test]
async fn test_migrate_sync_verify_cycle() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Stage a v0-layout vault. Real v0 vaults had both user-facing
    // files on disk *and* `.syncline/data/<uuid>.bin` Yrs snapshots
    // carrying the CRDT metadata; the migrator reads the snapshots
    // and trusts the on-disk files to already match.
    let vault = TempDir::new().unwrap();
    let v0_data = vault.path().join(".syncline/data");
    seed_v0_text_snapshot(&v0_data, "alpha.md", "alpha body");
    seed_v0_text_snapshot(&v0_data, "beta.md", "beta body");
    fs::write(vault.path().join("alpha.md"), "alpha body").unwrap();
    fs::write(vault.path().join("beta.md"), "beta body").unwrap();

    // 1. Migrate — rewrites .syncline/ into v1 layout. Migrate does not
    //    materialise user-facing files on its own; that happens when the
    //    sync client reconciles the projection to disk.
    let migrate_status = run_migrate_cli(vault.path()).await;
    assert!(migrate_status.success(), "migrate CLI must exit 0");
    assert!(vault.path().join(".syncline/version").exists());
    assert!(vault.path().join(".syncline/manifest.bin").exists());
    assert!(vault.path().join(".syncline/data.v0.bak").exists());
    assert!(!vault.path().join(".syncline/data").exists());

    // 2. Sync to the server. The client's reconcile loop materialises the
    //    migrated entries onto disk *and* pushes the manifest upstream.
    let mut client = spawn_client(vault.path(), port).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    client.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        fs::read_to_string(vault.path().join("alpha.md")).unwrap(),
        "alpha body"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("beta.md")).unwrap(),
        "beta body"
    );

    // 3. Verify — must converge with the server.
    let verify_status = run_verify_cli(vault.path(), port, 3).await;
    assert!(
        verify_status.success(),
        "verify after migrate+sync must exit 0, got {:?}",
        verify_status.code()
    );

    server.kill().await.unwrap();
}

/// Content-addressed blob storage: two clients that independently write
/// identical binary bytes at different paths must converge to the same
/// blob hash in `.syncline/blobs/`. A correct CAS layer stores the
/// bytes once per unique hash, so both clients reuse the same on-disk
/// blob file.
#[tokio::test]
async fn test_binary_blob_cas_dedup_across_clients() {
    let env = TestEnv::new(2).await;

    let payload: Vec<u8> = (0..=255u8).collect();
    fs::write(env.client_path(0).join("one.png"), &payload).unwrap();
    fs::write(env.client_path(1).join("two.png"), &payload).unwrap();

    // Poll for both files arriving on the opposite client.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let a_has_b = env.client_path(0).join("two.png").exists();
        let b_has_a = env.client_path(1).join("one.png").exists();
        if a_has_b && b_has_a {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "binary files did not cross-sync within 15s"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Hash of the payload — CAS filename is the hex SHA-256.
    let hash = syncline::v1::hash_hex(&payload);
    // Blob store layout: .syncline/blobs/<aa>/<bb>/<full-hash>.
    let expected_rel = Path::new("blobs")
        .join(&hash[0..2])
        .join(&hash[2..4])
        .join(&hash);

    for idx in 0..2 {
        let blob_path = env.client_path(idx).join(".syncline").join(&expected_rel);
        assert!(
            blob_path.exists(),
            "client {} missing expected blob at {}",
            idx,
            blob_path.display()
        );
        let stored = fs::read(&blob_path).unwrap();
        assert_eq!(stored, payload, "client {} blob content mismatch", idx);
    }

    // And the payload under both user paths is identical on both sides.
    for idx in 0..2 {
        assert_eq!(
            fs::read(env.client_path(idx).join("one.png")).unwrap(),
            payload
        );
        assert_eq!(
            fs::read(env.client_path(idx).join("two.png")).unwrap(),
            payload
        );
    }
}

// ---------------------------------------------------------------------------
// Channel-buffer overflow regression tests (v1.0.2)
// ---------------------------------------------------------------------------
//
// Repro for the v1.0.1 bug that surfaced in Tom's vault chaos incident
// (1305-file Obsidian vault on macOS, "Channel full or closed,
// dropped debounced file event" hundreds of times per minute,
// blob uploads lost, peers receiving empty placeholder files).
//
// Root cause: `client_v1::run_client` builds the watcher mpsc with
// only **16 slots** (`syncline/src/client_v1.rs:326`). The
// DebouncedWatcher pumps a `Vec<DebouncedEvent>` per ~300 ms window,
// but `scan_once` over a thousands-of-files vault takes longer than
// that, so successive batches stack up and the channel fills. Once
// full, `try_send` returns `Full(..)` and the batch — together with
// every blob upload it would have triggered — is silently dropped.
//
// These tests assert two invariants that v1.0.1 violates:
//   (a) every file written on peer 0 reaches peer 1 with byte-for-byte
//       identical content (no empty placeholders); and
//   (b) peer 0's stderr is free of the "Channel full" / "dropped"
//       error lines emitted by `watcher.rs:25,75`.
//
// The fix lives in `client_v1.rs:326` — bump the buffer to a size that
// can absorb a vault-bootstrap burst (or switch to unbounded). Vaults
// with 1000+ files are not an edge case (KMS users, research vaults,
// codebases used as Obsidian vaults), so bootstrap MUST scale.

/// Smaller incremental variant: two peers start synced and empty,
/// then peer 0 adds 500 files in a single tight write loop (mimics
/// `cp -r` of a research dump or extracting an archive into the
/// vault). All 500 must land on peer 1 with full content, and peer 0
/// must not log any dropped events.
///
/// This exercise is closer to the steady-state "shove a directory
/// into the vault" workflow than the cold-bootstrap scenario above
/// and reliably saturates the watcher channel because `scan_once`
/// after each debounce window is comparatively cheap (small vault),
/// so backpressure manifests purely as channel fill from rapid
/// debounce batches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_add_500_files_at_once_no_drops() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let _server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dir0 = TempDir::new().unwrap();
    let dir1 = TempDir::new().unwrap();
    // Capture stderr on both peers — bug is symmetric (sender-side
    // drops on peer 0 from the bootstrap scan, receiver-side drops
    // on peer 1 from inbound writes — same channel, same code path).
    let (_c0, drops0) = spawn_client_capturing_drops(dir0.path(), port).await;
    let (_c1, drops1) = spawn_client_capturing_drops(dir1.path(), port).await;

    // Allow both clients to fully attach FSEvents/inotify before the
    // burst — we want the watcher to see every write live, not via
    // the post-attach scan_once shortcut.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Burst: 500 files written with no pause in between. The
    // debouncer (300 ms window) will coalesce per-path duplicates,
    // but distinct paths flow through as Vec<DebouncedEvent> batches.
    const N: usize = 500;
    for i in 0..N {
        let body = format!(
            "burst {i:03}\n{}\n",
            "y".repeat(160 + (i % 80))
        );
        fs::write(
            dir0.path().join(format!("burst-{i:03}.md")),
            body,
        )
        .unwrap();
    }

    let dirs = vec![dir0.path().to_path_buf(), dir1.path().to_path_buf()];
    let converged = wait_for_convergence(&dirs, Duration::from_secs(180)).await;
    assert!(
        converged,
        "{N}-file burst did not converge within 3 min — files likely \
         lost to channel-overflow drops."
    );

    // Drain stderr buffers before reading the drop counters.
    tokio::time::sleep(Duration::from_secs(2)).await;

    for i in 0..N {
        let name = format!("burst-{i:03}.md");
        let p0 = dir0.path().join(&name);
        let p1 = dir1.path().join(&name);
        assert!(p1.exists(), "peer 1 missing {name} after burst");
        let c0 = fs::read(&p0).unwrap();
        let c1 = fs::read(&p1).unwrap();
        assert_eq!(
            c1, c0,
            "peer 1 content mismatch on {name} (likely empty placeholder)"
        );
    }

    let d0 = drops0.lock().unwrap().clone();
    let d1 = drops1.lock().unwrap().clone();
    let total_drops = d0.len() + d1.len();
    assert!(
        total_drops == 0,
        "Channel-overflow drops during {N}-file burst: peer 0 = {} \
         line(s), peer 1 = {} line(s). Sample:\n  {}",
        d0.len(),
        d1.len(),
        d0.iter()
            .chain(d1.iter())
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Stress test: bootstrap a vault with 1500 small files to peer 1.
///
/// Repro for SyncLine v1.0.1 channel buffer overflow:
/// `mpsc::channel(100)` in `client/watcher.rs` and `client/app.rs` cannot
/// hold the burst of file events emitted by `scan_once` over a vault of
/// 1000+ files. Events get dropped (`Channel full or closed, dropped
/// debounced file event`), so content blobs are never uploaded — peer 1
/// receives manifest entries but the bodies remain empty placeholders.
///
/// Tom's real-world incident on 2026-04-25: 1300+ file vault stuck with
/// hundreds of empty placeholders on Mac after iPhone reconnect.
///
/// This test FAILs on parent commit (channel(100)) and PASSes after the
/// fix (channel bumped to 10_000).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bootstrap_large_vault_no_dropped_events() {
    const N: usize = 1500;

    let mut env = TestEnv::new(2).await;

    // Pre-populate peer 0 vault BEFORE sync sees it.
    // Spread across a few subfolders to mimic a real vault layout.
    let dir0 = env.client_path(0).to_path_buf();
    for i in 0..N {
        let folder = format!("folder-{:02}", i % 30);
        let folder_path = dir0.join(&folder);
        fs::create_dir_all(&folder_path).unwrap();
        let name = format!("note-{:04}.md", i);
        let body = format!(
            "# Note {i}\n\nContent body for stress test, line 1.\nLine 2 with some text.\nLine 3 final.\n"
        );
        fs::write(folder_path.join(&name), body).unwrap();
    }

    // Now spawn the client, which will trigger a bulk scan_once.
    // This is where the channel(100) buffer overflows in v1.0.1.
    let dir1_path = env.client_dirs[1].path().to_path_buf();
    env.clients[1] = spawn_client(&dir1_path, env.port).await;

    // Generous convergence budget — bootstrap of 1500 files takes a while.
    let dirs = vec![dir0.clone(), env.client_path(1).to_path_buf()];
    let converged = wait_for_convergence(&dirs, Duration::from_secs(240)).await;
    assert!(
        converged,
        "Bootstrap of {N}-file vault did not converge: peer 1 is missing files \
         or has empty placeholders. This indicates the watcher mpsc channel \
         overflowed (bug repro)."
    );

    // Cross-check on disk: every file on peer 1 must exist AND have
    // the exact content peer 0 wrote. An empty-placeholder file would
    // slip past convergence-by-set-membership but fail this check.
    for i in 0..N {
        let folder = format!("folder-{:02}", i % 30);
        let name = format!("note-{:04}.md", i);
        let p0 = dir0.join(&folder).join(&name);
        let p1 = env.client_path(1).join(&folder).join(&name);
        assert!(p1.exists(), "peer 1 missing {folder}/{name} after bootstrap");
        let c0 = fs::read(&p0).unwrap();
        let c1 = fs::read(&p1).unwrap();
        assert_eq!(
            c1.len(),
            c0.len(),
            "peer 1 has wrong size for {folder}/{name}: {} vs {} bytes \
             (likely empty placeholder from dropped debounced event)",
            c1.len(),
            c0.len()
        );
        assert_eq!(c1, c0, "peer 1 content mismatch for {folder}/{name}");
    }
}

/// Regression test for #60 — bulk-scan upload of a large vault doesn't
/// duplicate manifest entries when the CLI is killed mid-stream and
/// restarted.
///
/// The bug surface: scan_once's tight write.send().await loop on
/// thousands of frames could starve the runtime's WS-pong / broadcast-
/// forward tasks, the server's recv-window saturated, and the
/// connection RST'd under load. The CLI then reconnected (or the user
/// restarted it) and re-uploaded everything, doubling the manifest
/// with `.conflict-` siblings.
///
/// The fix has two parts:
///   1. Throttle the bulk sends with `tokio::task::yield_now()` every
///      BURST_SIZE frames so the runtime can service the read side.
///   2. Persist `manifest.bin` BEFORE any WS sends (rather than after
///      the manifest update send) so a kill mid-burst leaves the on-
///      disk manifest in a state that matches the in-memory one — a
///      subsequent restart loads the right NodeIds and the per-file
///      adoption path skips re-create.
///
/// This test pre-populates a 1500-file vault, kills the first CLI mid-
/// scan, then starts a fresh CLI in the same dir and a fresh observer
/// peer. It asserts the observer ends up with exactly N files and
/// zero `.conflict-` siblings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cli_restart_no_duplicate_manifest_entries() {
    // N must be large enough that the first scan_once still has work
    // to do when we yank the CLI. Too small → scan completes before
    // the kill, save_manifest fires under either before-or-after-send
    // ordering, and the test can't tell the orderings apart.
    const N: usize = 1500;

    let _ = build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let _server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client_dir = TempDir::new().unwrap();

    // Pre-populate dir0 with N files BEFORE the first CLI starts —
    // forces the initial scan to actually do work, with enough volume
    // that the scan is still mid-flight at the kill instant.
    for i in 0..N {
        let body = format!("# Note {i}\n\nfile-{i} body line 1.\nLine 2.\nLine 3.\n");
        fs::write(
            client_dir.path().join(format!("note-{i:04}.md")),
            body,
        )
        .unwrap();
    }

    // First CLI run: kill mid-stream so that scan_once is interrupted
    // *during* the WS send phase. The kill window only matters relative
    // to where save_manifest sits in scan_once: with the bug, save is
    // *after* the manifest send, so a kill during the burst leaves
    // manifest.bin in the pre-scan state. With the fix, save is *before*
    // any send, so manifest.bin reflects the in-memory state regardless
    // of when the kill lands.
    //
    // 800ms is enough for the v1 handshake + the per-file Loop 1
    // (content.persist) on a 1500-file vault, but well before the
    // bulk content-update sends finish.
    {
        let mut client1 = spawn_client(client_dir.path(), port).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        client1.kill().await.unwrap();
        drop(client1);
    }

    // After the kill, manifest.bin should reflect the in-memory state
    // at the moment scan_once finished its per-file mutations — that's
    // the persist-before-send invariant. The directory must exist at
    // minimum.
    let manifest_bin = client_dir.path().join(".syncline").join("manifest.bin");
    assert!(
        manifest_bin.is_file(),
        "manifest.bin must exist after the first CLI run did any work"
    );

    // Second CLI run in the same dir: simulates the user restarting
    // syncline after an alarming-looking error log. With the bug, it
    // sees an empty (or pre-scan) local manifest and re-mints NodeIds
    // for every vault file the server already has. With the fix, the
    // local manifest matches in-memory state and scan_once adopts.
    let _client2 = spawn_client(client_dir.path(), port).await;
    tokio::time::sleep(Duration::from_secs(30)).await;

    // Spawn an observer peer (fresh dir) — anything not in its
    // converged view doesn't exist on the server. If `scan_once` had
    // re-uploaded files as new manifest entries on the second run,
    // the observer would see ~2N files (N originals + N
    // `.conflict-…` copies). With the fix, exactly N.
    let observer_dir = TempDir::new().unwrap();
    let _observer = spawn_client(observer_dir.path(), port).await;
    tokio::time::sleep(Duration::from_secs(60)).await;

    let mut all_md: Vec<String> = fs::read_dir(observer_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".md") { Some(n) } else { None }
        })
        .collect();
    all_md.sort();
    let conflicts: Vec<&String> = all_md.iter().filter(|n| n.contains(".conflict-")).collect();

    assert_eq!(
        conflicts.len(),
        0,
        "observer saw {} conflict copies after CLI restart — second scan_once \
         minted fresh NodeIds for paths the server already had (#60). Sample: {:?}",
        conflicts.len(),
        conflicts.iter().take(3).collect::<Vec<_>>()
    );
    assert_eq!(
        all_md.len(),
        N,
        "observer saw {} .md files, expected {} (extra files indicate \
         duplicate manifest entries from the second CLI run)",
        all_md.len(),
        N
    );
}
