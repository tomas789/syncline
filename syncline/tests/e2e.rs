use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tracing::error;
use yrs::{Any, GetString, Map, Out, Transact};

use std::sync::atomic::{AtomicU16, Ordering};

static NEXT_PORT: AtomicU16 = AtomicU16::new(18000);

pub fn get_available_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::Relaxed)
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

    // Yrs cache should exist — with UUID-based naming the .bin filename is a UUID,
    // so just verify at least one .bin file was created in the data directory.
    let data_dir = env.client_path(0).join(".syncline/data");
    let has_bin = fs::read_dir(&data_dir)
        .into_iter()
        .flatten()
        .any(|e| {
            e.ok()
                .map(|e| {
                    e.path().extension().and_then(|s| s.to_str()) == Some("bin")
                })
                .unwrap_or(false)
        });
    assert!(has_bin, "A .bin file should exist in .syncline/data/ after syncing test.md");

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
    let env = TestEnv::new(2).await;
    let binary0 = env.client_path(0).join("image.png");
    let hidden0 = env.client_path(0).join(".hidden.md");

    fs::write(&binary0, "binary data").unwrap();
    fs::write(&hidden0, "secret text").unwrap();

    tokio::time::sleep(Duration::from_millis(2000)).await;

    // Binary files (like .png) should now be synced via binary support
    assert!(env.client_path(1).join("image.png").exists(),
            ".png file should be synced with binary file support");
    // Hidden files (starting with .) should still NOT be synced
    assert!(!env.client_path(1).join(".hidden.md").exists(),
            ".hidden files should not be synced");
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

    // "note (client-b).md" should exist on B with B's original content
    let conflict_path_b = dir_b.path().join("note (client-b).md");
    assert!(
        conflict_path_b.exists(),
        "Conflict file 'note (client-b).md' should exist in dir_b"
    );
    assert_eq!(
        fs::read_to_string(&conflict_path_b).unwrap(),
        "content from B",
        "Conflict file should have B's original content"
    );

    // Restart client A and verify it receives the conflict file from the server
    let mut client_a2 = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    tokio::time::sleep(Duration::from_millis(4000)).await;

    let conflict_path_a = dir_a.path().join("note (client-b).md");
    assert!(
        conflict_path_a.exists(),
        "Client A should receive the conflict file 'note (client-b).md'"
    );
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
    // Wait until A has synced (state dir contains more than just __index__)
    // before starting B, to ensure B sees A's UUID in initial_server_uuids.
    let mut client_a2 = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    let state_dir = dir_a.path().join(".syncline").join("data");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if state_dir.exists() {
            let count = fs::read_dir(&state_dir)
                .map(|rd| rd.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
            // __index__.bin + at least one doc .bin = 2+ files
            if count >= 2 {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Timed out waiting for client A to sync its document"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // B reconnects — detects conflict, renames its content
    let mut client_b2 = spawn_client_with_name(dir_b.path(), port, "client-b").await;
    tokio::time::sleep(Duration::from_millis(8000)).await;

    // B's shared.md should now have A's content
    let shared_b = fs::read_to_string(dir_b.path().join("shared.md")).unwrap();
    assert_eq!(
        shared_b, "A's offline content",
        "shared.md on B should be replaced with A's (server) content"
    );

    // B's conflict file should contain B's original content
    let conflict_b = dir_b.path().join("shared (client-b).md");
    assert!(
        conflict_b.exists(),
        "Conflict file 'shared (client-b).md' should exist on B"
    );
    assert_eq!(fs::read_to_string(&conflict_b).unwrap(), "B's offline content");

    // A should eventually receive the conflict file
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let conflict_a = dir_a.path().join("shared (client-b).md");
    assert!(
        conflict_a.exists(),
        "Client A should receive the conflict file 'shared (client-b).md'"
    );
    assert_eq!(fs::read_to_string(&conflict_a).unwrap(), "B's offline content");

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
