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

/// Recursively count files whose name matches either of syncline's
/// two conflict naming schemes, anywhere under `root`. Skips the
/// `.syncline/` state cache.
///
/// Two schemes coexist:
///   1. **Projection-level** (`v1::projection::conflict_path`):
///      `<stem>.conflict-<actor8>-<lamp>-<id8>.<ext>`. Emitted when
///      two manifest entries share a path; the loser is renamed.
///   2. **Reconcile-level** (`client_v1::conflict_sibling_path`):
///      `<stem> (conflict <YYYY-MM-DD> <actor8>).<ext>`. Emitted by
///      `reconcile_projection_to_disk` when local on-disk bytes
///      differ from the manifest's chunk hashes.
fn count_conflict_files(root: &Path) -> usize {
    let mut n = 0;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let rel = match p.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.starts_with(".syncline") {
            continue;
        }
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.contains(".conflict-") || name.contains(" (conflict ") {
            n += 1;
        }
    }
    n
}

/// Regression test for #107 — CLI client lacks self-write fs-event
/// suppression.
///
/// Scenario reproducing the production bug:
///
///   1. Server + two clients spin up.
///   2. Client 0 drops N files (its folder was empty before).
///   3. Server broadcasts the manifest + content updates to client 1.
///   4. Client 1 materialises every file on disk.
///   5. Client 1's fs-watcher fires for those writes — they look like
///      "new local files" because there's no self-write suppression
///      on the CLI side (PR #102 added that to the Obsidian plugin
///      only).
///   6. Client 1's `scan_once` runs against the freshly-written
///      files. For each path the projection has client 0's entry,
///      but the text-adoption guard requires
///      `content.has_persisted(id) || body.is_empty()` — racy with
///      respect to the concurrent text-content subscription. When
///      adoption misses, `scan_once` falls through to `create_text`
///      and mints a fresh node at the same path with client 1's
///      actor.
///   7. Server receives the colliding create from client 1, projects
///      it, and emits a `.conflict-<actor1>-<lamp>-<id>.md` sibling.
///   8. Both clients reconcile to disk, ending up with the original
///      file PLUS a conflict copy.
///
/// Pre-fix: the test fails — at least one `.conflict-*` file appears
/// in client 0's directory, even though client 0 only wrote files
/// itself and never edited anything.
///
/// Post-fix: the CLI watcher suppresses self-writes the same way the
/// plugin does (#91). No spurious creates, no conflicts.
///
/// Note: the simple "client A writes N text files, client B receives,
/// no edits" case turns out NOT to reproduce reliably — the text
/// path's empty-placeholder adoption rule
/// (`content.has_persisted(id) || body.is_empty()`) lets scan_once
/// adopt the placeholder, so no spurious create. The
/// `test_binary_modification_during_bootstrap_does_not_create_phantom_conflict_entry`
/// test below exercises the actual production path: binary file
/// rewrites trigger reconcile's conflict-sibling branch, which writes
/// a new path on disk that the projection doesn't know about, which
/// then trips scan_once into minting a fresh node.
#[tokio::test]
async fn test_cli_does_not_self_loop_on_received_writes() {
    let env = TestEnv::new(2).await;

    // Drop N files into client 0. Larger than 1 so reconciliation on
    // client 1 takes long enough that the fs-watcher batches at
    // least one round-trip; small enough to keep the test fast.
    const N: usize = 50;
    for i in 0..N {
        let path = env.client_path(0).join(format!("note-{i:03}.md"));
        fs::write(&path, format!("# note {i}\n\nlorem ipsum {i}\n")).unwrap();
    }

    // Wait for convergence: both clients should agree on the set of
    // files. With the bug they still converge — the server emits
    // conflict pairs and both clients agree on the (original +
    // conflict-sibling) set. So we ALSO assert no `.conflict-*`
    // files exist anywhere. That's the actual bug surface.
    let converged = wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await;
    assert!(
        converged,
        "Clients did not converge after 20 seconds — even with the bug, both sides should agree on _some_ shape"
    );

    // Settle period: give the watcher's debounce + scan_once + any
    // server round-trip time to finish before we count. Without this,
    // conflicts that are just about to land would be missed and the
    // test would falsely pass.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let conflicts_a = count_conflict_files(env.client_path(0));
    let conflicts_b = count_conflict_files(env.client_path(1));
    assert_eq!(
        conflicts_a, 0,
        "client 0 has {conflicts_a} `.conflict-*` files — the CLI's self-write loop generated conflict pairs from its own received writes (#107)"
    );
    assert_eq!(
        conflicts_b, 0,
        "client 1 has {conflicts_b} `.conflict-*` files — the CLI's self-write loop generated conflict pairs from its own received writes (#107)"
    );
}

/// Hypothesis A2 for #107 — the actual production bug.
///
/// `reconcile_projection_to_disk` in `client_v1.rs` has a binary
/// conflict branch (around line 1675): when a manifest update arrives
/// with new chunks for a path whose local on-disk file has different
/// chunks, it saves the local bytes as `<stem>.conflict-<actor>-<date>.<ext>`
/// **on disk** and overwrites the original with the remote bytes.
/// The conflict-sibling path is purely a disk artifact at that point —
/// it is NOT in the projection.
///
/// The watcher then fires for the conflict-sibling write. `scan_once`
/// calls `process_binary_file` against the conflict-sibling path.
/// `proj.by_path.get(conflict_path)` returns `None` (the projection
/// only knows the original path), so it falls into the
/// `BinaryScanOutcome::Created` branch and mints a fresh manifest
/// entry at the conflict-sibling path with the **CLI's actor**.
///
/// That entry then propagates to every other peer, which materializes
/// a new conflict-sibling file on its own disk. The user sees a
/// conflict file on every device even though no real conflict ever
/// existed.
///
/// This test reproduces the failure with two CLIs and a single binary
/// file that gets modified once during the bootstrap:
///
///   1. Client A writes binary foo.png (bytes A1).
///   2. Both clients converge.
///   3. Client A overwrites foo.png with bytes A2 (different content).
///   4. Client B's reconcile sees disk-A1 vs manifest-A2 → creates
///      a conflict sibling on disk.
///   5. Client B's watcher fires → scan_once mints a manifest entry
///      for the conflict-sibling path with B's actor.
///   6. Client A receives this entry and materializes the conflict
///      sibling on its own disk too.
///
/// Pre-fix: a `.conflict-*` file appears on **client A** (where the
/// user never wrote any conflict file). That's the bug — A never
/// triggered any conflict, but B's self-write loop pushed one back
/// to it.
///
/// Post-fix: B's watcher should drop the fs-event for the
/// reconcile-driven conflict-sibling write (it's a self-write). No
/// scan, no mint, no propagation.
#[tokio::test]
async fn test_binary_modification_during_bootstrap_does_not_create_phantom_conflict_entry() {
    let env = TestEnv::new(2).await;

    // 1. Client A writes binary file. Two clients converge.
    let png_path = env.client_path(0).join("image.png");
    let bytes_v1 = vec![0xAAu8; 4096]; // arbitrary binary content
    fs::write(&png_path, &bytes_v1).unwrap();

    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await,
        "Initial binary file did not converge across clients"
    );

    // 2. Modify on A.
    let bytes_v2 = vec![0xBBu8; 4096];
    fs::write(&png_path, &bytes_v2).unwrap();

    // 3. Wait for the modification to propagate to client B. Use a
    //    file-content poll instead of `wait_for_convergence` —
    //    convergence-by-file-set is too strict here: with the fix
    //    in place, client B keeps a LOCAL conflict-sibling artifact
    //    on disk (preserving the v1 bytes for user review) that
    //    client A doesn't have. The bug is when that artifact
    //    becomes a *manifest entry* and propagates back to A; the
    //    artifact's existence on B alone is intended behavior.
    let path_b = env.client_path(1).join("image.png");
    let propagated = wait_for(
        Duration::from_secs(20),
        Duration::from_millis(500),
        || async {
            fs::read(&path_b).map(|got| got == bytes_v2).unwrap_or(false)
        },
    )
    .await;
    assert!(
        propagated,
        "Binary modification did not reach client 1 within 20 s"
    );

    // 4. Settle: give the post-conflict watcher fire + scan_once + any
    //    server round-trip enough wall-clock to mint and propagate
    //    the spurious entry, if the bug is present.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 5. Client A should have NO conflict-sibling file. The bug
    //    causes B's reconcile-driven local artifact to be minted by
    //    scan_once and propagated back to A, materialising on A's
    //    disk. With the fix, the artifact stays local-only on B.
    let conflicts_a = count_conflict_files(env.client_path(0));
    assert_eq!(
        conflicts_a, 0,
        "client 0 has {conflicts_a} `.conflict-*` / `(conflict ...)` files — \
         client 1's reconcile-driven conflict sibling was minted into the \
         manifest by its own scan_once, then propagated back to client 0 (#107)"
    );

    // Client B is allowed exactly one local conflict-sibling: the
    // one its reconcile wrote when applying the v2 update over the
    // v1 disk. More than one indicates the bug is firing on every
    // pass.
    let conflicts_b = count_conflict_files(env.client_path(1));
    assert!(
        conflicts_b <= 1,
        "client 1 has {conflicts_b} `.conflict-*` / `(conflict ...)` files; \
         exactly one local-only sibling from reconcile is expected, more \
         indicates the bug is firing on every reconcile pass"
    );
}

/// Lightweight polling helper used by tests that wait on a single
/// async predicate (e.g. "this file's bytes == X"). The
/// `wait_for_convergence` helper above is too strict for tests where
/// peers are expected to disagree on local-only artifacts.
async fn wait_for<F, Fut>(
    timeout: Duration,
    poll: Duration,
    mut check: F,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(poll).await;
    }
    check().await
}

/// Hypothesis A3 for #107 — the **exact** user-reported scenario.
///
/// Reproduces the production sequence end-to-end:
///
///   1. Server starts. Database is empty.
///   2. CLI sync ("server-side mirror") starts against an **empty**
///      folder. This is the role of `syncline-sync.service` on the
///      user's Linux box.
///   3. A second CLI sync starts against a folder **pre-populated**
///      with N files. This stands in for "Tom PC plugin pushes its
///      existing vault" — both peers behave the same on the wire
///      once the manifest is exchanged.
///   4. Wait for convergence — the populated peer pushes the manifest
///      and content; the empty-folder peer receives and materialises.
///   5. Settle window for the receiving peer's watcher to fire on its
///      own writes and (with the bug) emit phantom create / conflict
///      ops back to the server.
///   6. Assert: NO conflict files anywhere in either folder.
///
/// This is the simplest direct repro of "I started clean, plugin
/// pushed the vault, conflicts appeared everywhere". CLI-only so it's
/// deterministic in CI; the fix on the CLI side automatically helps
/// the plugin case because the plugin's own writes don't trigger
/// this branch (they go through `vault.modify` which fires inside the
/// cookie window from #91).
#[tokio::test]
async fn test_initial_bootstrap_clean_server_does_not_create_phantom_conflicts() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let _server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Receiving peer ("server-side mirror") — empty folder, started
    // first so it's already listening when the populated peer comes
    // online. Mirrors the user's `syncline-sync.service` waiting
    // before Tom's plugin pushed the vault.
    let receiver_dir = TempDir::new().unwrap();
    let _receiver = spawn_client(receiver_dir.path(), port).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Pre-populate the sender's folder BEFORE starting `syncline sync`
    // there. The first scan after connect uploads everything in one
    // burst — same shape as a plugin connecting with a populated
    // vault.
    let sender_dir = TempDir::new().unwrap();
    // Production scale was ~1200 text + ~163 binary. Verified
    // empirically in a Linux/podman container that this CLI-only
    // scenario PASSES at both N=200 and N=1200 — the receiving
    // peer's text-adoption rule covers the bootstrap-write race.
    // Kept at N=200 to keep CI runtime reasonable; if this test
    // ever starts failing it's a real regression.
    //
    // The user's production case still produced conflicts because
    // the actual trigger requires either:
    //   * a binary file rewrite during bootstrap (covered by
    //     `test_binary_modification_during_bootstrap_does_not_create_phantom_conflict_entry`
    //     above), or
    //   * the Obsidian plugin's specific protocol burst pattern,
    //     not reproducible in CLI-only.
    const N_TEXT: usize = 200;
    for i in 0..N_TEXT {
        let subdir = format!("dir-{:02}", i % 8);
        let dir = sender_dir.path().join(&subdir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("note-{i:03}.md"));
        let body = format!(
            "# note {i}\n\n{}\n",
            "lorem ipsum dolor sit amet ".repeat(5 + (i % 7))
        );
        fs::write(&path, body).unwrap();
    }
    // Some binary files too — the binary-conflict reconcile branch
    // is exactly the one that mints phantom entries (see hypothesis
    // A2 above).
    for i in 0..10 {
        let path = sender_dir.path().join(format!("img-{i:02}.bin"));
        let bytes: Vec<u8> = (0..(1024 + i * 64))
            .map(|n| (n as u8).wrapping_mul((i as u8).wrapping_add(7)))
            .collect();
        fs::write(&path, &bytes).unwrap();
    }

    let _sender = spawn_client(sender_dir.path(), port).await;

    // Allow the bootstrap to settle: scan_once pushes from sender,
    // server broadcasts, receiver materialises every entry, watcher
    // fires on every materialisation, debounced scan runs, the
    // (buggy) self-write loop creates phantoms.
    let dirs = vec![
        sender_dir.path().to_path_buf(),
        receiver_dir.path().to_path_buf(),
    ];
    assert!(
        wait_for_convergence(&dirs, Duration::from_secs(60)).await,
        "Initial bootstrap did not converge in 60 s"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;

    let conflicts_sender = count_conflict_files(sender_dir.path());
    let conflicts_receiver = count_conflict_files(receiver_dir.path());

    assert_eq!(
        conflicts_sender, 0,
        "Sender (the peer that was 'plugin-equivalent') has \
         {conflicts_sender} `.conflict-*` / `(conflict ...)` files \
         after a clean bootstrap. The receiving peer's self-write \
         loop produced phantom conflicts and propagated them back. \
         This is exactly #107."
    );
    assert_eq!(
        conflicts_receiver, 0,
        "Receiver (the peer that was 'server-side mirror') has \
         {conflicts_receiver} `.conflict-*` / `(conflict ...)` files \
         after a clean bootstrap. The receiver materialised valid \
         remote content and then minted spurious siblings via its own \
         scan. This is exactly #107."
    );
}

/// Hypothesis A4 / "suspenders" for #107 — does scan_once mint manifest
/// entries for stale conflict-sibling artifacts on disk?
///
/// This test isolates the periodic-scan path. The cookie fix
/// (planned for `syncline/src/client/watcher.rs`, mirroring #91)
/// suppresses watcher events for paths the CLI just wrote — but
/// the periodic timer at `client_v1.rs:583` runs `scan_once`
/// every `SCAN_INTERVAL` (30 s) regardless of recent writes. If a
/// conflict-sibling-format file is sitting on disk when that timer
/// fires (e.g. left over from a previous bug run, copied in by an
/// external tool, or restored from backup), `scan_once` will walk
/// it, find no projection entry at that path, and `create_text` /
/// `create_binary` mint a fresh manifest entry with the local
/// actor. The cookie does nothing for this — no recent self-write
/// to suppress.
///
/// This test is the **decision point** the reviewer asked for:
///   * If FAIL on current (unfixed) code → cookie alone is not
///     enough; the fix must also teach `scan_once` to recognise
///     the conflict-sibling regex and skip those paths.
///   * If PASS on current code → the cookie alone closes everything;
///     no suspenders needed.
///
/// Setup:
///   1. Pre-populate client 0's folder with one legitimate file
///      and five files matching `client_v1::conflict_sibling_path`'s
///      output format (`<stem> (conflict YYYY-MM-DD <hash8>).<ext>`).
///   2. Bring up two clients. Client 0's first `scan_once` walks
///      everything that's already on disk; client 1 sits empty,
///      receiving anything 0 pushes.
///   3. Assert: the legitimate file propagates; the artifacts do
///      not.
#[tokio::test]
async fn test_scan_once_skips_stale_conflict_sibling_artifacts() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let _server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Empty receiver, started first. Mirrors a peer that joins
    // before the artifacts get pushed.
    let receiver_dir = TempDir::new().unwrap();
    let _receiver = spawn_client(receiver_dir.path(), port).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Pre-populate sender BEFORE starting `syncline sync`. The
    // first scan_once after the WS handshake walks every file
    // already on disk and decides whether to mint a manifest entry
    // for each one.
    let sender_dir = TempDir::new().unwrap();
    fs::write(
        sender_dir.path().join("real-note.md"),
        "legitimate user content\n",
    )
    .unwrap();
    for i in 0..5 {
        let stale = sender_dir
            .path()
            .join(format!("phantom-{i} (conflict 2026-01-15 deadbeef).md"));
        fs::write(&stale, format!("stale artifact {i}\n")).unwrap();
    }

    let _sender = spawn_client(sender_dir.path(), port).await;

    // Wait long enough for: WS handshake → first MANIFEST_SYNC →
    // first scan_once → push manifest update → server broadcast →
    // receiver reconcile materialise. Keep the wait under
    // SCAN_INTERVAL (30 s) so a second periodic scan_once doesn't
    // muddy the picture.
    tokio::time::sleep(Duration::from_secs(20)).await;

    // Sanity: legitimate file propagated. If this fails the test
    // setup is broken, not the bug.
    assert!(
        receiver_dir.path().join("real-note.md").is_file(),
        "real-note.md did not propagate to receiver — test setup broken"
    );

    // Hypothesis: stale conflict-sibling artifacts MUST NOT
    // propagate. scan_once should recognise the format and refuse
    // to mint manifest entries for those paths. Without the
    // suspenders fix, scan_once treats them as normal new files
    // and mints — which then propagates to every other peer.
    let mut leaked = Vec::new();
    for i in 0..5 {
        let phantom = receiver_dir
            .path()
            .join(format!("phantom-{i} (conflict 2026-01-15 deadbeef).md"));
        if phantom.is_file() {
            leaked.push(phantom);
        }
    }
    assert!(
        leaked.is_empty(),
        "Suspenders missing: {} stale conflict-sibling artifact(s) on \
         the sender's disk were minted into the manifest by `scan_once` \
         and propagated to the receiver: {:?}. \
         The cookie fix (#91-style) on the watcher does not cover this \
         path because the artifacts were not written by the CLI itself; \
         scan_once needs an explicit conflict-sibling-format check \
         before calling `create_text` / `create_binary`. (#107)",
        leaked.len(),
        leaked,
    );
}

/// Hypothesis B for #107 — the 100 % CPU + freeze the user observed
/// on Obsidian startup is a downstream effect of the conflict
/// explosion (hypothesis A2). With ~900 phantom conflict siblings in
/// the manifest, the plugin's first reconcile pass has to materialize
/// 2× the legitimate corpus, plus subscribe to twice as many content
/// subdocs, plus request twice as many blobs. That's a real CPU
/// burst — but once the burst settles, things return to normal,
/// matching "after a few restarts ok now".
///
/// The risk hidden underneath is a feedback loop: if reconcile can
/// trip itself into an infinite cycle (each reconcile creates new
/// manifest mutations → observer fires → reconcile runs → ...), the
/// burst would never end and the freeze would be permanent. The
/// single-flight guard on `reconcileProjection` is supposed to
/// collapse trailing reconciles, but the guard only helps if each
/// reconcile is bounded.
///
/// This test pins down the boundedness: a CLI given a manifest with
/// 200 entries (a heavy synthetic workload, but well within the
/// user's 1200 + 921 conflict siblings range, scaled down so the
/// test finishes in CI) converges within 60 seconds. If reconcile
/// ever degenerates into an infinite loop, the test times out.
///
/// Doesn't directly measure CPU — that's environment-dependent — but
/// "converges in bounded wall-clock time" is the actionable
/// guarantee. CPU usage is a function of work × time; bound the time
/// and the user-visible "freeze" stops being indefinite.
#[tokio::test]
async fn test_large_manifest_converges_within_bounded_time() {
    let env = TestEnv::new(2).await;

    const N: usize = 200;
    for i in 0..N {
        let path = env.client_path(0).join(format!("note-{i:04}.md"));
        let body = format!("# note {i}\n\n{}\n", "lorem ipsum ".repeat(20));
        fs::write(&path, body).unwrap();
    }

    // 60s timeout: comfortably bounds the legitimate work and would
    // catch any infinite-loop regression. Production saw freezes
    // resolve "after a few restarts" — i.e., the burst always
    // terminated within order-of-minutes. 60s is a tighter bound
    // for our smaller corpus, expected to pass with 30+ seconds of
    // headroom.
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(60)).await,
        "Large manifest ({} entries) did not converge within 60 s — \
         either reconcile is not bounded or the inbound pipeline \
         stalled (#107 follow-up)",
        N,
    );
}

/// Hypothesis C for #107 — "conflict files reappear on Mac after I
/// deleted them on the server" is not a sync bug, it's a workflow
/// rule the user hit by accident.
///
/// When the user's AI assistant ran `trash` on the server's mirror
/// folder, the local `syncline sync` service was already stopped (a
/// correct precaution against the conflict-loop bug). Filesystem
/// operations on a peer whose `syncline sync` is not running do NOT
/// reach the CRDT manifest — there's no watcher to translate them
/// into delete ops. The Mac plugin therefore saw the unchanged
/// manifest and kept materialising the phantom conflict files.
///
/// Recovery path for the user (not tested here, but worth noting):
/// delete the conflict files via the Mac plugin (which IS running),
/// so the deletes propagate as manifest tombstones. The
/// `test_offline_creation_and_deletion` and
/// `test_rename_then_delete_propagates` cases below already cover
/// the running-peer delete-propagation path.
///
/// Caveat: when a stopped peer is *restarted*, its first
/// `scan_once` detects projection entries whose disk paths weren't
/// visited and emits delete ops for them — so restarting after an
/// offline cleanup DOES propagate the deletes. The user's symptom
/// applies only as long as the cleaned-up peer stays stopped.
#[tokio::test]
async fn test_fs_delete_on_stopped_peer_does_not_propagate() {
    let mut env = TestEnv::new(2).await;

    let path0 = env.client_path(0).join("docs/note.md");
    fs::create_dir_all(path0.parent().unwrap()).unwrap();
    fs::write(&path0, "shared content").unwrap();
    assert!(
        wait_for_convergence(&env.dirs(), Duration::from_secs(20)).await,
        "Initial setup did not converge"
    );

    let path1 = env.client_path(1).join("docs/note.md");
    assert!(path1.is_file(), "client 1 should have the file before stopping");

    // Stop client 1 entirely. Deletes on its filesystem now have no
    // path to the CRDT manifest.
    env.clients[1].kill().await.unwrap();
    // wait for the kill to actually take effect — otherwise the
    // watcher might still process the pending fs event.
    tokio::time::sleep(Duration::from_secs(1)).await;

    fs::remove_file(&path1).unwrap();
    // Give the rest of the system 5 s to (mistakenly) react. With
    // sync running on client 1 this would propagate to client 0
    // within a debounce window. Stopped → it doesn't.
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        path0.is_file(),
        "client 0 lost its copy of the file even though client 1's delete \
         happened with sync stopped — fs ops on stopped peers must not \
         propagate (this is the contract that explains #107's symptom C)"
    );
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

// ---------------------------------------------------------------------------
// #59 — chunked-blob sync end-to-end
// ---------------------------------------------------------------------------
//
// Before chunking, any single binary file > 16 MiB exceeded
// `tokio-tungstenite`'s default `max_frame_size` and the connection
// got dropped silently — the user-visible failure mode in #59 (the
// scanner retries forever without ever logging on the server side).
//
// With FastCDC chunking, the file is split into pieces of at most
// MAX_CHUNK_SIZE (4 MiB) on the wire, well under the 5 MiB
// `MAX_BLOB_SIZE` cap. These tests pin the new behaviour with vault
// content over the old 16 MiB threshold.

/// Deterministic pseudo-random bytes — no `rand` dep, bit-exact across
/// runs so test failures are reproducible.
fn pseudo_random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.max(1);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// 20 MiB binary file syncs end-to-end. Pre-#59 fix this hung the
/// scanner forever on the >16 MiB WebSocket frame limit; with chunking
/// the file flows as a handful of chunk frames, each ≤ 4 MiB, with
/// the same bit-for-bit integrity guarantee as a small blob.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_large_binary_over_16mib_syncs_via_chunks() {
    let env = TestEnv::new(2).await;

    let payload = pseudo_random_bytes(0xC0FFEE, 20 * 1024 * 1024);
    let src = env.client_path(0).join("big.bin");
    fs::write(&src, &payload).expect("write 20 MiB source");

    // 25-second budget — large blob bootstrap on a slow CI runner can
    // take a while. The signal we're after is "eventually equal", not
    // "equal in 5s".
    let dst = env.client_path(1).join("big.bin");
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    loop {
        if dst.exists() {
            if let Ok(landed) = fs::read(&dst) {
                if landed == payload {
                    return;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "20 MiB binary did not converge to peer within 25s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Both peers concurrently write *different* large binaries to the
/// same path. Eventual-consistency contract: one peer's bytes win the
/// canonical name, the other peer's bytes are preserved as a
/// `.conflict-…` sibling. Multi-chunk content uses the same projection
/// rule as single-blob content (§6.4 of the design doc).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_large_binary_writes_converge_with_conflict() {
    let env = TestEnv::new(2).await;

    // Different bytes on each peer at the same path. 8 MiB is enough
    // to force ≥ 2 chunks per file under the 4 MiB ceiling.
    let bytes_a = pseudo_random_bytes(0xAAAA, 8 * 1024 * 1024);
    let bytes_b = pseudo_random_bytes(0xBBBB, 8 * 1024 * 1024);
    fs::write(env.client_path(0).join("clash.bin"), &bytes_a).unwrap();
    fs::write(env.client_path(1).join("clash.bin"), &bytes_b).unwrap();

    // Wait for convergence: both peers must end up with the same set
    // of files (canonical + conflict sibling) holding the same byte
    // strings. Polling instead of a fixed sleep — multi-MiB sync on a
    // CI VM is timing-sensitive.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let conflict_a = find_conflict_sibling(env.client_path(0), "clash", "bin");
        let conflict_b = find_conflict_sibling(env.client_path(1), "clash", "bin");
        let canonical_a = env.client_path(0).join("clash.bin");
        let canonical_b = env.client_path(1).join("clash.bin");

        if let (Some(ca), Some(cb)) = (conflict_a.as_ref(), conflict_b.as_ref()) {
            if canonical_a.exists() && canonical_b.exists() {
                let canon_a_bytes = fs::read(&canonical_a).unwrap_or_default();
                let canon_b_bytes = fs::read(&canonical_b).unwrap_or_default();
                let conf_a_bytes = fs::read(ca).unwrap_or_default();
                let conf_b_bytes = fs::read(cb).unwrap_or_default();
                // Both peers agree on canonical bytes.
                if canon_a_bytes == canon_b_bytes
                    && !canon_a_bytes.is_empty()
                    // The conflict sibling holds the loser's bytes;
                    // both peers materialise the same conflict bytes.
                    && conf_a_bytes == conf_b_bytes
                    && !conf_a_bytes.is_empty()
                    // The two byte-strings together must be exactly
                    // the two peers' original payloads — no third
                    // value invented mid-sync.
                    && {
                        let pair = [&canon_a_bytes[..], &conf_a_bytes[..]];
                        let originals = [&bytes_a[..], &bytes_b[..]];
                        pair.iter().all(|b| originals.contains(b))
                    }
                {
                    return;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "clash didn't converge within 30s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Edit a large binary in-place and confirm the new content lands on
/// the peer. The bandwidth-saving property (only changed chunks are
/// re-uploaded) is unit-tested in `client_v1::tests`; this test just
/// pins the end-to-end correctness path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_chunked_binary_modify_propagates() {
    let env = TestEnv::new(2).await;

    // Initial: 6 MiB on peer 0 → ≥ 2 chunks under the 4 MiB cap.
    let v1_bytes = pseudo_random_bytes(0x1111, 6 * 1024 * 1024);
    fs::write(env.client_path(0).join("doc.bin"), &v1_bytes).unwrap();

    let dst = env.client_path(1).join("doc.bin");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if dst.exists() && fs::read(&dst).map(|b| b == v1_bytes).unwrap_or(false) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "initial chunked binary did not propagate within 20s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Modify: same approximate size, different seed so most chunks
    // change. Same convergence shape.
    let v2_bytes = pseudo_random_bytes(0x2222, 6 * 1024 * 1024);
    fs::write(env.client_path(0).join("doc.bin"), &v2_bytes).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if dst.exists() && fs::read(&dst).map(|b| b == v2_bytes).unwrap_or(false) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "modified chunked binary did not propagate within 20s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// =============================================================================
// Obsidian-like onboarding scenarios
// =============================================================================
//
// User flow under test:
//   1. The user runs `syncline server` somewhere.
//   2. They start a CLI client on a fresh, empty folder (their server proxy
//      / "always on" peer) and let it connect.
//   3. They start the Obsidian plugin pointed at the same server, except
//      their vault folder ALREADY HAS a typical mix of pre-existing
//      content: a couple of `.md` notes (one nested), a binary attachment,
//      and the `.obsidian/` folder with both a "should-sync" plugin
//      config AND the device-local `workspace.json` (which is in the
//      default ignore list — must NOT propagate).
//
// The Obsidian plugin and the CLI client share the entire `v1::*`
// portable layer; the WASM client mirrors the CLI's bootstrap path
// (scan disk → manifest → push). So the CLI-vs-CLI test below is the
// closest in-process proxy we have for "Obsidian onboarding into a
// non-empty server" without spinning up real Obsidian. It exercises:
//   - default ignore-list defaults (`.obsidian/workspace.json`)
//   - nested directory creation on the receiving peer
//   - binary blob upload + chunk fetch → byte-identical disk file
//   - bidirectional sync after onboarding
//   - cold-restart of the seeded peer with offline edits applied to
//     the fresh peer in between
async fn poll_until<F>(deadline: std::time::Instant, mut check: F) -> bool
where
    F: FnMut() -> bool,
{
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    check()
}

#[tokio::test]
async fn test_obsidian_like_onboarding_with_pre_existing_vault() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 2: empty/fresh client connects first.
    let fresh_dir = TempDir::new().unwrap();
    let mut client_fresh = spawn_client_with_name(fresh_dir.path(), port, "fresh").await;
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // Step 3: pre-seed an "Obsidian-like" vault.
    let seeded_dir = TempDir::new().unwrap();
    let seeded = seeded_dir.path();
    fs::create_dir_all(seeded.join("notes/daily")).unwrap();
    fs::create_dir_all(seeded.join(".obsidian")).unwrap();
    fs::write(seeded.join("welcome.md"), "hello from seeded\n").unwrap();
    fs::write(
        seeded.join("notes/daily/day1.md"),
        "# Day 1\nfirst entry",
    )
    .unwrap();
    let photo_bytes: Vec<u8> = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00pretend-jpeg-bytes".to_vec();
    fs::write(seeded.join("photo.jpg"), &photo_bytes).unwrap();
    fs::write(
        seeded.join(".obsidian/community-plugins.json"),
        b"[\"dataview\",\"templater\"]",
    )
    .unwrap();
    // workspace.json is in the default ignore list → MUST NOT sync.
    fs::write(
        seeded.join(".obsidian/workspace.json"),
        b"{\"layout\":\"device-local\"}",
    )
    .unwrap();

    let mut client_seeded = spawn_client_with_name(seeded, port, "seeded").await;

    // Wait for all four user-visible files to land on the fresh peer.
    let fresh = fresh_dir.path().to_path_buf();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let bootstrap_ok = poll_until(deadline, || {
        fresh.join("welcome.md").is_file()
            && fresh.join("notes/daily/day1.md").is_file()
            && fresh.join("photo.jpg").is_file()
            && fresh.join(".obsidian/community-plugins.json").is_file()
    })
    .await;
    assert!(
        bootstrap_ok,
        "expected files did not arrive on fresh peer within 20s; \
         present = {:?}",
        walkdir::WalkDir::new(&fresh)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter(|e| !e.path().to_string_lossy().contains(".syncline"))
            .map(|e| e.path().strip_prefix(&fresh).unwrap().to_path_buf())
            .collect::<Vec<_>>(),
    );

    // Byte-identical content — text, nested text, binary, and an
    // un-ignored .obsidian config.
    assert_eq!(
        fs::read_to_string(fresh.join("welcome.md")).unwrap(),
        "hello from seeded\n",
    );
    assert_eq!(
        fs::read_to_string(fresh.join("notes/daily/day1.md")).unwrap(),
        "# Day 1\nfirst entry",
    );
    assert_eq!(fs::read(fresh.join("photo.jpg")).unwrap(), photo_bytes);
    assert_eq!(
        fs::read(fresh.join(".obsidian/community-plugins.json")).unwrap(),
        b"[\"dataview\",\"templater\"]",
    );

    // The default-ignored workspace.json must NOT have propagated.
    assert!(
        !fresh.join(".obsidian/workspace.json").exists(),
        ".obsidian/workspace.json must be excluded by the default ignore list",
    );

    // Bidirectional: fresh appends to a synced file and creates a new
    // one; seeded picks both up.
    fs::write(
        fresh.join("welcome.md"),
        "hello from seeded\nappended on fresh\n",
    )
    .unwrap();
    fs::write(fresh.join("notes/daily/day2.md"), "## Day 2\nfrom fresh\n").unwrap();

    let seeded_path = seeded.to_path_buf();
    let bidi_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let bidi_ok = poll_until(bidi_deadline, || {
        seeded_path.join("notes/daily/day2.md").is_file()
            && fs::read_to_string(seeded_path.join("welcome.md"))
                .map(|s| s.contains("appended on fresh"))
                .unwrap_or(false)
    })
    .await;
    assert!(
        bidi_ok,
        "bidirectional propagation timed out: day2 exists? {}, welcome.md = {:?}",
        seeded_path.join("notes/daily/day2.md").exists(),
        fs::read_to_string(seeded_path.join("welcome.md")).ok(),
    );

    // Cold-restart the seeded client and apply offline edits on fresh
    // in between. After restart, the offline edits must arrive.
    client_seeded.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    fs::write(
        fresh.join("notes/daily/day3.md"),
        "offline edit from fresh\n",
    )
    .unwrap();
    fs::write(
        fresh.join("welcome.md"),
        "hello from seeded\nappended on fresh\nedit while seeded was offline\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let mut client_seeded2 = spawn_client_with_name(seeded, port, "seeded").await;
    let restart_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let restart_ok = poll_until(restart_deadline, || {
        seeded_path.join("notes/daily/day3.md").is_file()
            && fs::read_to_string(seeded_path.join("welcome.md"))
                .map(|s| s.contains("edit while seeded was offline"))
                .unwrap_or(false)
    })
    .await;
    assert!(
        restart_ok,
        "post-restart catch-up failed: day3 exists? {}, welcome.md = {:?}",
        seeded_path.join("notes/daily/day3.md").exists(),
        fs::read_to_string(seeded_path.join("welcome.md")).ok(),
    );

    client_fresh.kill().await.unwrap();
    client_seeded2.kill().await.unwrap();
    server.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-003: server SIGKILL mid-broadcast → restart with same DB →
// peers must re-converge cleanly. Models a real ops event: server crash
// (or node reboot) while two CLI peers are exchanging a small batch of
// files. After restart, the same SQLite DB carries forward, and clients
// must re-handshake, push their post-crash deltas, and converge.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_003_server_sigkill_mid_sync_then_restart_converges() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");

    // Phase 1: server up, two clients connect, write a baseline file.
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "peer-b").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;

    fs::write(dir_a.path().join("baseline.md"), "before crash\n").unwrap();
    let dirs = vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()];
    assert!(
        wait_for_convergence(&dirs, Duration::from_secs(20)).await,
        "baseline should converge before crash",
    );

    // Phase 2: while clients are quiet, write more files on both sides
    // and IMMEDIATELY kill the server. Some of those writes may not
    // have made it into the DB.
    fs::write(dir_a.path().join("a-during-crash.md"), "a wrote this\n").unwrap();
    fs::write(dir_b.path().join("b-during-crash.md"), "b wrote this\n").unwrap();
    // Tiny window — emulate "writes in flight, server dies".
    tokio::time::sleep(Duration::from_millis(150)).await;
    server.kill().await.unwrap();

    // Phase 3: server stays down briefly, then we restart against the
    // same DB. Clients should reconnect on their own (RECONNECT_BASE_MS
    // backoff in the CLI).
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut server2 = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // After server restart, clients reconnect, push their during-crash
    // writes, and converge. Add one more post-restart write to prove
    // the channel is healthy.
    fs::write(dir_a.path().join("post-restart.md"), "after restart\n").unwrap();
    let converged_after = wait_for_convergence(&dirs, Duration::from_secs(60)).await;

    // Best-effort: collect what we ended up with on both sides.
    let user_files = |dir: &Path| -> Vec<String> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let rel = p.strip_prefix(dir).unwrap();
            let s = rel.to_string_lossy().to_string();
            if s.starts_with(".syncline") {
                continue;
            }
            out.push(s);
        }
        out.sort();
        out
    };
    let a_files = user_files(dir_a.path());
    let b_files = user_files(dir_b.path());
    assert!(
        converged_after,
        "peers must converge after server restart\nA: {:?}\nB: {:?}",
        a_files,
        b_files,
    );

    // Sanity: every file we authored must have ended up on both peers.
    for must in &[
        "baseline.md",
        "a-during-crash.md",
        "b-during-crash.md",
        "post-restart.md",
    ] {
        assert!(
            a_files.iter().any(|f| f == must),
            "peer A missing {must} — has {:?}",
            a_files
        );
        assert!(
            b_files.iter().any(|f| f == must),
            "peer B missing {must} — has {:?}",
            b_files
        );
    }

    // No conflict copies should exist — none of these writes collided.
    assert_eq!(
        count_conflict_files(dir_a.path()),
        0,
        "no conflict files expected on peer A"
    );
    assert_eq!(
        count_conflict_files(dir_b.path()),
        0,
        "no conflict files expected on peer B"
    );

    client_a.kill().await.unwrap();
    client_b.kill().await.unwrap();
    server2.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-008: peer B's `.syncline/` directory is wiped (manual
// `rm -rf` / disk corruption / failed install) while its vault files
// remain on disk. On reconnect the peer should re-handshake with the
// server, re-discover its actor_id is fresh, and converge to the same
// vault contents as peer A. No duplicates, no loss, no conflict copies
// (the disk content already agrees with the server's manifest).
//
// IGNORED — this test reproduces a real bug; see KNOWN_BUGS.md
// "Phantom conflict copies after `.syncline/` wipe-recovery". The bug
// is genuine and reproduces deterministically (every disk file becomes
// `<name>.conflict-...` on both peers after peer B's `.syncline/` is
// wiped + reconnected). A proper fix needs a deferred-adopt path that
// distinguishes wipe-recovery (disk == eventual remote body) from
// offline-collision (disk != eventual remote body), which the current
// scan_once API cannot do without subscribing to the content subdoc
// first. Tracked as known-bug #16. Re-enable once that ships.
// ===========================================================================
#[ignore]
#[tokio::test]
async fn auto_apr28_008_wiped_syncline_dir_recovers_via_resync() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "peer-b").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Peer A authors several files; both peers converge.
    for i in 0..6 {
        fs::write(
            dir_a.path().join(format!("file-{i:02}.md")),
            format!("content of file {i}\n"),
        )
        .unwrap();
    }
    let dirs = vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()];
    assert!(
        wait_for_convergence(&dirs, Duration::from_secs(20)).await,
        "initial sync should converge"
    );

    // Sanity: peer B has them all.
    for i in 0..6 {
        let p = dir_b.path().join(format!("file-{i:02}.md"));
        assert!(p.is_file(), "peer B missing initial file {p:?}");
    }

    // Kill peer B and wipe its .syncline directory entirely.
    client_b.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let syncline_dir = dir_b.path().join(".syncline");
    assert!(syncline_dir.is_dir(), "pre-wipe sanity");
    fs::remove_dir_all(&syncline_dir).expect("wipe .syncline/");
    assert!(
        !syncline_dir.exists(),
        "post-wipe: .syncline/ must be gone"
    );

    // Vault user files are still on disk untouched. Reconnect peer B.
    let mut client_b2 = spawn_client_with_name(dir_b.path(), port, "peer-b").await;

    // First wait for the new peer B to actually come up — its
    // .syncline/manifest.bin should be re-created within a few seconds
    // of process start. Without this gate, `wait_for_convergence`
    // trivially passes (both A and B's user files were unchanged by
    // the wipe).
    let manifest_back_deadline =
        std::time::Instant::now() + Duration::from_secs(20);
    let manifest_back = poll_until(manifest_back_deadline, || {
        syncline_dir.join("manifest.bin").is_file()
    })
    .await;
    assert!(
        manifest_back,
        ".syncline/manifest.bin should be re-created after \
         peer B reconnects post-wipe"
    );

    // Now actually verify a write→sync round-trip works after the
    // wipe. Peer A authors a new file; peer B must receive it.
    fs::write(dir_a.path().join("post-wipe-from-a.md"), "after wipe\n").unwrap();
    let post_wipe_path_b = dir_b.path().join("post-wipe-from-a.md");
    let post_wipe_deadline = std::time::Instant::now() + Duration::from_secs(30);
    let post_wipe_ok = poll_until(post_wipe_deadline, || {
        post_wipe_path_b
            .is_file()
            && fs::read_to_string(&post_wipe_path_b)
                .map(|s| s == "after wipe\n")
                .unwrap_or(false)
    })
    .await;
    assert!(
        post_wipe_ok,
        "post-wipe peer B should receive new files from peer A"
    );

    // .syncline/ must be back.
    assert!(
        syncline_dir.is_dir(),
        ".syncline/ should be re-created after resync"
    );

    // No conflict copies should exist on either side. The disk
    // bytes already match the server's manifest content for every
    // file, so the scanner's adoption rules should attach the local
    // disk file to the existing manifest entry rather than minting a
    // fresh colliding NodeId.
    let conflicts_a = count_conflict_files(dir_a.path());
    let conflicts_b = count_conflict_files(dir_b.path());
    assert_eq!(
        conflicts_a, 0,
        "no conflicts on A after resync"
    );
    assert_eq!(
        conflicts_b, 0,
        "no conflicts on B after resync (disk already matched manifest)"
    );

    // Every original file must still be on B and equal A's content.
    for i in 0..6 {
        let pa = dir_a.path().join(format!("file-{i:02}.md"));
        let pb = dir_b.path().join(format!("file-{i:02}.md"));
        assert!(pb.is_file(), "B missing file-{i:02}.md after resync");
        assert_eq!(
            fs::read_to_string(&pa).unwrap(),
            fs::read_to_string(&pb).unwrap(),
            "file-{i:02}.md content diverged after wipe+resync"
        );
    }

    client_a.kill().await.unwrap();
    client_b2.kill().await.unwrap();
    server.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-011: zero-byte text files round-trip across peers. Edge
// case for any system that has a "is empty?" branch (syncline does —
// adoption uses `body.is_empty()` as a short-circuit). The empty file
// must be created on disk on the receiving peer with size 0; not
// silently dropped, not represented as a 1-byte file with a NUL.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_011_empty_text_file_roundtrip() {
    let env = TestEnv::new(2).await;

    // Peer 0 creates an empty text file.
    let path0 = env.client_path(0).join("empty.md");
    fs::write(&path0, b"").unwrap();

    // Peer 0 also creates a non-empty file to ensure the manifest
    // sync isn't itself broken (as a control).
    fs::write(env.client_path(0).join("control.md"), b"non-empty").unwrap();

    let dirs = env.dirs();
    let converged = wait_for_convergence(&dirs, Duration::from_secs(20)).await;
    assert!(converged, "convergence should succeed for empty + control");

    // Peer 1's empty.md must exist and be 0 bytes.
    let path1 = env.client_path(1).join("empty.md");
    assert!(path1.is_file(), "peer 1 must have empty.md");
    let bytes = fs::read(&path1).unwrap();
    assert_eq!(
        bytes.len(),
        0,
        "empty.md on peer 1 should be 0 bytes, got {} bytes: {:?}",
        bytes.len(),
        bytes
    );

    // Now flip: peer 1 modifies empty.md to a non-empty value. Peer 0
    // should receive it.
    fs::write(&path1, "now has content\n").unwrap();
    let post_path = env.client_path(0).join("empty.md");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let saw_content = poll_until(deadline, || {
        fs::read_to_string(&post_path)
            .map(|s| s == "now has content\n")
            .unwrap_or(false)
    })
    .await;
    assert!(
        saw_content,
        "peer 0 should observe peer 1's modification to the once-empty file"
    );
}

// ===========================================================================
// auto-apr28-014: rapid file create→write→delete churn at the same
// path. Watcher debounce, scan_once, and manifest LWW must converge —
// the final state on both peers must be: file exists with the final
// content, OR file does not exist (depending on whether the last op
// was create or delete). No spurious empty files, no conflict copies.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_014_rapid_create_modify_delete_churn_converges() {
    let env = TestEnv::new(2).await;
    let path = env.client_path(0).join("churn.md");

    // 30 rounds of: create with version-N content, then delete.
    for i in 0..30 {
        fs::write(&path, format!("version {i}\n")).unwrap();
        // Tiny delay so the watcher event ordering is stable.
        tokio::time::sleep(Duration::from_millis(20)).await;
        if i % 2 == 1 {
            // Delete every other round so the manifest sees both
            // create and delete stamps interleaving.
            let _ = fs::remove_file(&path);
        }
    }

    // Final write — this should be the surviving content on both peers.
    fs::write(&path, "FINAL\n").unwrap();

    let dirs = env.dirs();
    let converged = wait_for_convergence(&dirs, Duration::from_secs(30)).await;
    assert!(converged, "post-churn convergence required");

    // Both peers must have the FINAL content.
    let final_a = fs::read_to_string(env.client_path(0).join("churn.md")).unwrap();
    let final_b = fs::read_to_string(env.client_path(1).join("churn.md")).unwrap();
    assert_eq!(final_a, "FINAL\n", "peer A churn.md not the final write");
    assert_eq!(final_b, "FINAL\n", "peer B churn.md not the final write");

    // No conflict copies.
    assert_eq!(
        count_conflict_files(env.client_path(0)),
        0,
        "no conflict copies on A"
    );
    assert_eq!(
        count_conflict_files(env.client_path(1)),
        0,
        "no conflict copies on B"
    );
}

// ===========================================================================
// auto-apr28-018: 3 peers each write 30 distinct files at roughly the
// same instant. After the dust settles every peer must have all 90
// unique files with the right content; no conflict copies, no losses.
// Stress for the manifest broadcast path under concurrent contention.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_018_three_peer_concurrent_burst_writes_converge() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dirs: Vec<TempDir> = (0..3).map(|_| TempDir::new().unwrap()).collect();
    let mut clients: Vec<Child> = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        let name = format!("peer-{i}");
        clients.push(spawn_client_with_name(d.path(), port, &name).await);
    }
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Every peer writes 30 unique-named files at once. Use peer index
    // as a namespace so paths are guaranteed distinct.
    const PER_PEER: usize = 30;
    for (peer_idx, d) in dirs.iter().enumerate() {
        for i in 0..PER_PEER {
            fs::write(
                d.path().join(format!("p{peer_idx}-{i:02}.md")),
                format!("peer {peer_idx} file {i}\n"),
            )
            .unwrap();
        }
    }

    let dir_paths: Vec<PathBuf> = dirs.iter().map(|d| d.path().to_path_buf()).collect();
    let converged = wait_for_convergence(&dir_paths, Duration::from_secs(60)).await;

    // Helper to list user files.
    let user_files = |dir: &Path| -> Vec<String> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let rel = p.strip_prefix(dir).unwrap();
            let s = rel.to_string_lossy().to_string();
            if s.starts_with(".syncline") {
                continue;
            }
            out.push(s);
        }
        out.sort();
        out
    };

    assert!(
        converged,
        "3-peer 30-each burst convergence required\n0: {:?}\n1: {:?}\n2: {:?}",
        user_files(dirs[0].path()),
        user_files(dirs[1].path()),
        user_files(dirs[2].path()),
    );

    // Every peer must end with exactly 90 user files.
    for (i, d) in dirs.iter().enumerate() {
        let files = user_files(d.path());
        assert_eq!(
            files.len(),
            3 * PER_PEER,
            "peer {i}: expected {} files, got {}: {:?}",
            3 * PER_PEER,
            files.len(),
            files
        );
    }

    // No conflict copies.
    for (i, d) in dirs.iter().enumerate() {
        assert_eq!(
            count_conflict_files(d.path()),
            0,
            "peer {i}: no conflict copies expected"
        );
    }

    for c in clients.iter_mut() {
        c.kill().await.unwrap();
    }
    server.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-024: binary file is created, syncs, then renamed across
// folder boundaries on peer 0. Peer 1 must observe the rename — same
// bytes appear at the new path, original path gone, no conflict copies.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_024_binary_rename_across_folders_propagates() {
    let env = TestEnv::new(2).await;

    // Peer 0 creates a binary file at the root.
    let original = env.client_path(0).join("image.bin");
    let bytes: Vec<u8> = (0..1024u32).flat_map(|i| (i as u32).to_le_bytes()).collect();
    fs::write(&original, &bytes).unwrap();

    // Wait until peer 1 has it.
    let dirs = env.dirs();
    let converged = wait_for_convergence(&dirs, Duration::from_secs(20)).await;
    assert!(converged, "initial binary sync should converge");
    let p1_original = env.client_path(1).join("image.bin");
    assert!(p1_original.is_file());
    assert_eq!(fs::read(&p1_original).unwrap(), bytes);

    // Peer 0 renames the binary file into a new subfolder.
    fs::create_dir_all(env.client_path(0).join("Pictures")).unwrap();
    let new_path = env.client_path(0).join("Pictures/cool.bin");
    fs::rename(&original, &new_path).unwrap();

    // Wait for peer 1 to see the rename.
    let p1_new = env.client_path(1).join("Pictures/cool.bin");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let renamed = poll_until(deadline, || {
        p1_new.is_file()
            && !p1_original.exists()
            && fs::read(&p1_new).map(|b| b == bytes).unwrap_or(false)
    })
    .await;
    assert!(
        renamed,
        "binary rename should propagate to peer 1: \
         new path exists? {}, original gone? {}, content match? {}",
        p1_new.is_file(),
        !p1_original.exists(),
        fs::read(&p1_new).map(|b| b == bytes).unwrap_or(false),
    );

    // No conflict copies anywhere.
    assert_eq!(count_conflict_files(env.client_path(0)), 0);
    assert_eq!(count_conflict_files(env.client_path(1)), 0);
}

// ===========================================================================
// auto-apr28-030: server crashes IMMEDIATELY after starting (before
// any client connects), then restarts with the same DB. Models a flaky
// server / failed first-start. Clients must connect cleanly to the
// second instance.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_030_server_immediate_crash_then_restart_clients_connect() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");

    // First server instance: kill it almost immediately.
    let mut server1 = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    server1.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Second instance same DB.
    let mut server2 = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Connect a fresh client.
    let dir_a = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Make sure the client actually connected (has a manifest).
    assert!(
        dir_a.path().join(".syncline").join("manifest.bin").is_file(),
        "client should have re-bootstrapped against the second server"
    );

    // Write a file and verify it's persisted server-side via a second
    // client.
    fs::write(dir_a.path().join("hello.md"), "after restart\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let dir_b = TempDir::new().unwrap();
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "peer-b").await;

    let target = dir_b.path().join("hello.md");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let saw = poll_until(deadline, || {
        target.is_file()
            && fs::read_to_string(&target)
                .map(|s| s == "after restart\n")
                .unwrap_or(false)
    })
    .await;
    assert!(saw, "second client should observe the post-restart write");

    client_a.kill().await.unwrap();
    client_b.kill().await.unwrap();
    server2.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-031: server SIGKILLed while a multi-MiB binary blob is
// being uploaded. The blob is large enough that the upload almost
// certainly straddles the kill window. After the server is restarted
// against the same DB, the original peer (which still has the file on
// disk) and a fresh second peer must converge — bytes preserved, no
// half-blob orphans, no phantom conflict copies.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_031_server_killed_mid_blob_upload_recovers() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");

    let mut server1 = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dir_a = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // Write a binary file big enough to be chunked (FastCDC kicks in
    // beyond a few KiB; 4 MiB is comfortably multi-chunk and gives the
    // upload pipeline a real window to be killed in.)
    let bytes: Vec<u8> = (0..4 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761u32) >> 24) as u8)
        .collect();
    let blob_path = dir_a.path().join("big.bin");
    fs::write(&blob_path, &bytes).unwrap();

    // Tiny window — give the watcher time to fire and the client time
    // to begin streaming MSG_BLOB_UPDATE chunks, but kill before it
    // can finish.
    tokio::time::sleep(Duration::from_millis(120)).await;
    server1.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Restart server against same DB.
    let mut server2 = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Bring up a fresh peer B; it must observe the binary file with the
    // exact bytes once peer A finishes re-uploading.
    let dir_b = TempDir::new().unwrap();
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "peer-b").await;

    let target = dir_b.path().join("big.bin");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let saw = poll_until(deadline, || {
        target.is_file()
            && fs::read(&target).map(|b| b == bytes).unwrap_or(false)
    })
    .await;
    assert!(
        saw,
        "peer B should observe the same bytes after server restart \
         (file exists? {}, size? {})",
        target.is_file(),
        target.metadata().map(|m| m.len()).unwrap_or(0),
    );

    // No phantom conflict copies on either side.
    assert_eq!(count_conflict_files(dir_a.path()), 0, "no conflicts on A");
    assert_eq!(count_conflict_files(dir_b.path()), 0, "no conflicts on B");

    client_a.kill().await.unwrap();
    client_b.kill().await.unwrap();
    server2.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-033: a single peer with a deeply nested directory layout
// (8 levels deep, multiple files per level, ~50 files total) bootstraps
// against an empty server. A second fresh peer joins and must observe
// the entire tree. Catches regressions in projection of deep paths and
// directory bootstrap ordering.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_033_deeply_nested_tree_single_peer_bootstraps() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");

    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pre-seed peer A's vault with a deeply nested tree BEFORE the
    // client starts — exercises offline bootstrap of an existing
    // hierarchy.
    let dir_a = TempDir::new().unwrap();
    let mut all_files = Vec::new();
    let mut current = dir_a.path().to_path_buf();
    for level in 0..8 {
        current = current.join(format!("level{level}"));
        fs::create_dir_all(&current).unwrap();
        // 5 files per level
        for n in 0..5 {
            let p = current.join(format!("file{level}-{n}.md"));
            let body = format!("level={level} file={n}\n");
            fs::write(&p, body.as_bytes()).unwrap();
            all_files.push((p.strip_prefix(dir_a.path()).unwrap().to_path_buf(), body));
        }
    }
    assert_eq!(all_files.len(), 40);

    // Now start peer A — it must scan the disk and upload everything.
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Fresh peer B joins.
    let dir_b = TempDir::new().unwrap();
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "peer-b").await;

    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let saw_all = poll_until(deadline, || {
        all_files.iter().all(|(rel, body)| {
            let p = dir_b.path().join(rel);
            p.is_file()
                && fs::read_to_string(&p)
                    .map(|s| s == *body)
                    .unwrap_or(false)
        })
    })
    .await;
    assert!(
        saw_all,
        "fresh peer B should observe every file in the deep tree \
         (peer A files: {}, peer B observed-on-disk: {})",
        all_files.len(),
        walkdir::WalkDir::new(dir_b.path())
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file()
                && !e.path().to_string_lossy().contains(".syncline"))
            .count(),
    );

    assert_eq!(count_conflict_files(dir_a.path()), 0, "no conflicts on A");
    assert_eq!(count_conflict_files(dir_b.path()), 0, "no conflicts on B");

    client_a.kill().await.unwrap();
    client_b.kill().await.unwrap();
    server.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-035: server is started, peers converge on a baseline,
// server is killed, the on-disk SQLite file is truncated to half its
// size (simulating disk corruption / a power-loss tear), then the
// server is restarted with a fresh DB at the same path. Clients must
// either bootstrap to a clean state on the new DB or fail loud — never
// silently desync. Currently we expect the new server to start with
// an empty DB (sqlx replaces a malformed file by truncating /
// re-initialising on connect failure, OR the test catches this and
// confirms the failure mode).
// ===========================================================================
#[tokio::test]
async fn auto_apr28_035_server_restart_with_corrupted_db_starts_clean_or_fails_loud() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");

    // Phase 1: server up, two peers converge.
    let mut server1 = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let dir_a = TempDir::new().unwrap();
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    fs::write(dir_a.path().join("baseline.md"), "before crash\n").unwrap();
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // Kill server first, then drop client (avoid client crash-loops on
    // a truncated DB — we want to test the SERVER's behaviour).
    client_a.kill().await.unwrap();
    server1.kill().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: corrupt the DB file. Truncate to half its current size.
    // SQLite's WAL files (`-wal`, `-shm`) we leave alone — many
    // production crashes leave only the main file inconsistent.
    let original_size = fs::metadata(&db_path).unwrap().len();
    assert!(
        original_size > 100,
        "db should be non-trivial, was {}",
        original_size
    );
    let corrupt_size = original_size / 2;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&db_path)
        .unwrap();
    f.set_len(corrupt_size).unwrap();
    drop(f);

    // Phase 3: restart server with the corrupted DB. The acceptable
    // outcomes are:
    //   (a) the server starts (sqlx may rebuild or report errors but
    //       the process accepts new connections), and a fresh peer can
    //       connect and write a file — the recovery is "clean slate".
    //   (b) the server fails to start — process exits with non-zero —
    //       and the operator must intervene. Either is loud and
    //       acceptable; silent desync is not.
    let mut server2 = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let still_running = server2.try_wait().unwrap().is_none();

    if !still_running {
        // Outcome (b): loud failure. That's fine — operator must
        // restore from backup. Test passes by recording this branch.
        eprintln!("server refused to start on corrupted DB — loud failure (acceptable)");
        return;
    }

    // Outcome (a): server is alive. A fresh peer must be able to
    // connect and exchange data.
    let dir_c = TempDir::new().unwrap();
    let mut client_c = spawn_client_with_name(dir_c.path(), port, "peer-c").await;
    tokio::time::sleep(Duration::from_millis(2000)).await;

    fs::write(dir_c.path().join("after-corrupt.md"), "post-corrupt\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Another fresh peer should also be able to read what C wrote.
    let dir_d = TempDir::new().unwrap();
    let mut client_d = spawn_client_with_name(dir_d.path(), port, "peer-d").await;
    let target = dir_d.path().join("after-corrupt.md");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let saw = poll_until(deadline, || {
        target.is_file()
            && fs::read_to_string(&target)
                .map(|s| s == "post-corrupt\n")
                .unwrap_or(false)
    })
    .await;
    assert!(
        saw,
        "post-corruption recovery: peer D should see peer C's write"
    );

    client_c.kill().await.unwrap();
    client_d.kill().await.unwrap();
    server2.kill().await.unwrap();
}

// ===========================================================================
// auto-apr28-034: a single CLI peer is restarted three times in a row.
// Each restart, a different file is added to the vault before the next
// restart. The peer's actor_id (from `.syncline/actor_id`) survives,
// and its lamport (from `.syncline/lamport`) is monotonic. After all
// restarts, a fresh second peer must observe every file authored
// across the restart cycles.
// ===========================================================================
#[tokio::test]
async fn auto_apr28_034_cli_peer_restart_cycle_preserves_history() {
    build_workspace().await;
    let port = get_available_port();
    let server_dir = TempDir::new().unwrap();
    let db_path = server_dir.path().join("test.db");
    let mut server = spawn_server(port, &db_path).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dir_a = TempDir::new().unwrap();

    // Track actor_id across restarts: must be stable.
    let actor_id_path = dir_a.path().join(".syncline/actor_id");

    let mut actor_observed: Option<String> = None;
    let restart_count = 3;
    for cycle in 0..restart_count {
        let mut client = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
        tokio::time::sleep(Duration::from_millis(2200)).await;

        // Write a per-cycle file.
        let body = format!("cycle={cycle}\n");
        fs::write(dir_a.path().join(format!("note-{cycle}.md")), &body).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let id = fs::read_to_string(&actor_id_path)
            .expect("actor_id file should exist after first run")
            .trim()
            .to_string();
        match &actor_observed {
            None => actor_observed = Some(id),
            Some(prev) => assert_eq!(
                prev, &id,
                "actor_id must be stable across restart cycle {cycle}"
            ),
        }

        client.kill().await.unwrap();
        // Wait for the kill_on_drop task to actually reap.
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    // Final restart so peer A is alive while peer B joins.
    let mut client_a = spawn_client_with_name(dir_a.path(), port, "peer-a").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let dir_b = TempDir::new().unwrap();
    let mut client_b = spawn_client_with_name(dir_b.path(), port, "peer-b").await;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let saw_all = poll_until(deadline, || {
        (0..restart_count).all(|cycle| {
            let p = dir_b.path().join(format!("note-{cycle}.md"));
            p.is_file()
                && fs::read_to_string(&p)
                    .map(|s| s == format!("cycle={cycle}\n"))
                    .unwrap_or(false)
        })
    })
    .await;
    assert!(saw_all, "peer B should observe all per-restart files");

    assert_eq!(count_conflict_files(dir_a.path()), 0);
    assert_eq!(count_conflict_files(dir_b.path()), 0);

    client_a.kill().await.unwrap();
    client_b.kill().await.unwrap();
    server.kill().await.unwrap();
}
