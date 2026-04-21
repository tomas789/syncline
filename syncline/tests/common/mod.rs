//! Shared test harness for filesystem-operation integration tests.
//!
//! Mirrors the inline helpers in `tests/e2e.rs` but factored into a module so
//! multiple test files can reuse the same server/client spawn + convergence
//! machinery without duplicating it.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use yrs::{Any, GetString, Map, Out, Transact};

static BASE_PORT: OnceLock<u16> = OnceLock::new();
static PORT_OFFSET: AtomicU16 = AtomicU16::new(0);

/// Returns a port unique to this (test_binary, call_count). The base is
/// derived from the process PID so parallel test binaries do not collide.
pub fn get_available_port() -> u16 {
    let base = *BASE_PORT.get_or_init(|| {
        // Map PID into the 16384..32768 range, aligned to 256 to leave room
        // for ~256 ports per test binary.
        0x4000u16 | ((std::process::id() as u16 & 0x3f) << 8)
    });
    base + PORT_OFFSET.fetch_add(1, Ordering::Relaxed)
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
        .env("RUST_LOG", "info")
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
        .env("RUST_LOG", "info")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn client")
}

/// Reads the `meta.path → content` map from a client's `.syncline/data/*.bin`
/// snapshots. For binary files, value is `"[binary]"`.
fn load_yrs_map(dir: &Path) -> HashMap<String, String> {
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
            continue;
        };
        let doc = yrs::Doc::new();
        {
            let mut txn = doc.transact_mut();
            txn.apply_update(update);
        }
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
            let t = doc.get_or_insert_text("content");
            let txn = doc.transact();
            result.insert(rel_path, GetString::get_string(&t, &txn));
        }
    }
    result
}

pub fn collect_user_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir).min_depth(1) {
        let Ok(entry) = entry else { continue };
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

/// Compares all client directories against client[0] as the reference.
/// Returns true only if every client has the same set of user files with
/// identical byte-level content, and identical CRDT content for text docs.
pub fn compare_directories(client_dirs: &[PathBuf]) -> bool {
    if client_dirs.is_empty() {
        return true;
    }

    let mut expected_files: HashMap<String, Vec<u8>> = HashMap::new();
    let expected_yrs = load_yrs_map(&client_dirs[0]);

    for entry in walkdir::WalkDir::new(&client_dirs[0]).min_depth(1) {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let path_str = path.to_string_lossy();
        if path_str.contains(".syncline") || path_str.contains(".git") {
            continue;
        }
        if path.is_file() {
            let rel = path.strip_prefix(&client_dirs[0]).unwrap();
            let name = rel.to_string_lossy().into_owned();
            let Ok(content) = fs::read(path) else { continue };
            expected_files.insert(name, content);
        }
    }

    for dir in client_dirs.iter().skip(1) {
        let mut actual_files: HashMap<String, Vec<u8>> = HashMap::new();
        let actual_yrs = load_yrs_map(dir);

        for entry in walkdir::WalkDir::new(dir).min_depth(1) {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let path_str = path.to_string_lossy();
            if path_str.contains(".syncline") || path_str.contains(".git") {
                continue;
            }
            if path.is_file() {
                let rel = path.strip_prefix(dir).unwrap();
                let name = rel.to_string_lossy().into_owned();
                let Ok(content) = fs::read(path) else { continue };
                actual_files.insert(name, content);
            }
        }

        let expected_keys: HashSet<&String> = expected_files.keys().collect();
        let actual_keys: HashSet<&String> = actual_files.keys().collect();

        if expected_keys != actual_keys {
            return false;
        }

        for (name, content) in &actual_files {
            if let Some(expected_content) = expected_files.get(name) {
                if content != expected_content {
                    return false;
                }
            }
        }
        for (rel_path, content) in &actual_yrs {
            if let Some(expected_content) = expected_yrs.get(rel_path) {
                if content != expected_content {
                    return false;
                }
            }
        }
    }

    true
}

pub async fn wait_for_convergence(dirs: &[PathBuf], timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if compare_directories(dirs) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    compare_directories(dirs)
}

/// Polls until `predicate()` returns true or the deadline expires.
pub async fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    predicate()
}

pub struct TestEnv {
    pub server_dir: TempDir,
    pub client_dirs: Vec<TempDir>,
    pub port: u16,
    pub server: Child,
    pub clients: Vec<Child>,
}

impl TestEnv {
    pub async fn new(num_clients: usize) -> Self {
        build_workspace().await;
        let port = get_available_port();
        let server_dir = TempDir::new().unwrap();
        let db_path = server_dir.path().join("test.db");
        let server = spawn_server(port, &db_path).await;

        // Let server bind
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut client_dirs = Vec::new();
        let mut clients = Vec::new();
        for _ in 0..num_clients {
            let dir = TempDir::new().unwrap();
            let client = spawn_client(dir.path(), port).await;
            client_dirs.push(dir);
            clients.push(client);
        }

        // Give clients time to connect and watchers to attach
        tokio::time::sleep(Duration::from_millis(2500)).await;

        Self {
            server_dir,
            client_dirs,
            port,
            server,
            clients,
        }
    }

    pub fn client_path(&self, idx: usize) -> &Path {
        self.client_dirs[idx].path()
    }

    pub fn dirs(&self) -> Vec<PathBuf> {
        self.client_dirs
            .iter()
            .map(|d| d.path().to_path_buf())
            .collect()
    }

    pub async fn restart_client(&mut self, idx: usize) {
        let dir = self.client_dirs[idx].path().to_path_buf();
        self.clients[idx] = spawn_client(&dir, self.port).await;
    }
}
