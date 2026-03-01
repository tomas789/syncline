use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tracing::error;
use yrs::Transact;

use std::sync::atomic::{AtomicU16, Ordering};

static NEXT_PORT: AtomicU16 = AtomicU16::new(18000);

pub fn get_available_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::Relaxed)
}

pub async fn build_workspace() {
    let status = Command::new("cargo")
        .args(["build", "--workspace"])
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

    let load_yrs = |dir: &PathBuf, doc_id: &str| -> String {
        let bin_path = dir.join(".syncline/data").join(format!("{}.bin", doc_id));
        if let Ok(content) = fs::read(&bin_path)
            && let Ok(update) = yrs::updates::decoder::Decode::decode_v1(&content)
        {
            let doc = yrs::Doc::new();
            let t = doc.get_or_insert_text("content");
            let mut txn = doc.transact_mut();
            txn.apply_update(update);
            return yrs::GetString::get_string(&t, &txn);
        }
        "".to_string()
    };

    let mut expected_files: HashMap<String, String> = HashMap::new();
    let mut expected_yrs: HashMap<String, String> = HashMap::new();

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
            let content = fs::read_to_string(path).unwrap();
            expected_files.insert(name.clone(), content);
            expected_yrs.insert(name.clone(), load_yrs(&client_dirs[0], &name));
        }
    }

    let mut converged = true;

    for (idx, dir) in client_dirs.iter().enumerate().skip(1) {
        let mut actual_files: HashMap<String, String> = HashMap::new();
        let mut actual_yrs: HashMap<String, String> = HashMap::new();

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
                let content = fs::read_to_string(path).unwrap();
                actual_files.insert(name.clone(), content);
                actual_yrs.insert(name.clone(), load_yrs(dir, &name));
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
                    "DISK File {} mismatches between Client 0 and Client {}.\nClient 0: {:?}\nClient {}: {:?}",
                    name, idx, expected_content, idx, content
                );
                converged = false;
            }
        }

        for (name, content) in &actual_yrs {
            if let Some(expected_content) = expected_yrs.get(name)
                && content != expected_content
            {
                error!(
                    "YRS File {} mismatches between Client 0 and Client {}.\nClient 0: {:?}\nClient {}: {:?}",
                    name, idx, expected_content, idx, content
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

    // Yrs cache should exist
    assert!(
        env.client_path(0)
            .join(".syncline/data/test.md.bin")
            .exists()
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
    let env = TestEnv::new(2).await;
    let ignored0 = env.client_path(0).join("ignored.png");
    let hidden0 = env.client_path(0).join(".hidden.md");

    fs::write(&ignored0, "binary data").unwrap();
    fs::write(&hidden0, "secret text").unwrap();

    tokio::time::sleep(Duration::from_millis(2000)).await;

    // Neither should appear in client 1
    assert!(!env.client_path(1).join("ignored.png").exists());
    assert!(!env.client_path(1).join(".hidden.md").exists());
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

    // A reconnects first — its content becomes server truth
    let mut client_a2 = spawn_client_with_name(dir_a.path(), port, "client-a").await;
    tokio::time::sleep(Duration::from_millis(3000)).await;

    // B reconnects — detects conflict, renames its content
    let mut client_b2 = spawn_client_with_name(dir_b.path(), port, "client-b").await;
    tokio::time::sleep(Duration::from_millis(5000)).await;

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
