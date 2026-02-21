use anyhow::{Context, Result};
use clap::Parser;
use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::Command;
use tracing::{debug, error, info};
use yrs::Transact;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = 3)]
    clients: usize,

    #[arg(short, long, default_value_t = 42)]
    seed: u64,

    #[arg(short, long, default_value_t = 15)]
    duration_secs: u64,

    #[arg(long, default_value_t = 3000)]
    port: u16,
}

async fn build_binaries() -> Result<()> {
    info!("Building workspace...");
    let status = Command::new("cargo")
        .args(["build", "--workspace"])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("cargo build failed");
    }
    Ok(())
}

fn mutate_content(rng: &mut StdRng, original: &str) -> String {
    let mut chars: Vec<char> = original.chars().collect();
    let num_mutations = rng.gen_range(1..=5);
    for _ in 0..num_mutations {
        let action = rng.gen_range(0..3);
        match action {
            0 => {
                // Insert
                if chars.is_empty() {
                    chars.push(rng.gen_range(b'a'..=b'z') as char);
                } else {
                    let idx = rng.gen_range(0..=chars.len());
                    chars.insert(idx, rng.gen_range(b'a'..=b'z') as char);
                }
            }
            1 => {
                // Delete
                if !chars.is_empty() {
                    let idx = rng.gen_range(0..chars.len());
                    chars.remove(idx);
                }
            }
            2 => {
                // Replace
                if !chars.is_empty() {
                    let idx = rng.gen_range(0..chars.len());
                    chars[idx] = rng.gen_range(b'a'..=b'z') as char;
                }
            }
            _ => unreachable!(),
        }
    }
    chars.into_iter().collect()
}

async fn runner_loop(
    client_id: usize,
    dir: PathBuf,
    seed: u64,
    running: Arc<AtomicBool>,
) -> Result<()> {
    // We deterministicly seed based on client_id and global seed
    let mut rng = StdRng::seed_from_u64(seed + client_id as u64);
    let files = vec!["fileA.md", "fileB.md", "fileC.md"];

    info!("Client {} started fuzzing loop", client_id);

    while running.load(Ordering::Relaxed) {
        let file = *files.choose(&mut rng).unwrap();
        let path = dir.join(file);

        let content = fs::read_to_string(&path).unwrap_or_default();
        let new_content = mutate_content(&mut rng, &content);

        // Introduce random line breaks sometimes
        let final_content = if rng.gen_bool(0.1) {
            new_content + "\nnew line " + &rng.gen_range(0..1000).to_string()
        } else {
            new_content
        };

        if let Err(e) = fs::write(&path, &final_content) {
            error!("Client {} failed to write: {}", client_id, e);
        } else {
            debug!("Client {} wrote to {}", client_id, file);
        }

        // Wait a random short duration to simulate user edits and burst writes
        let delay_ms = rng.gen_range(10..200);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }

    info!("Client {} finished fuzzing loop", client_id);
    Ok(())
}

fn compare_directories(client_dirs: &[PathBuf]) -> Result<bool> {
    if client_dirs.is_empty() {
        return Ok(true);
    }

    let load_yrs = |dir: &PathBuf, doc_id: &str| -> String {
        let bin_path = dir.join(".syncline/data").join(format!("{}.bin", doc_id));
        if let Ok(content) = fs::read(&bin_path) {
            if let Ok(update) = yrs::updates::decoder::Decode::decode_v1(&content) {
                let doc = yrs::Doc::new();
                let t = doc.get_or_insert_text("content");
                let mut txn = doc.transact_mut();
                txn.apply_update(update).unwrap();
                return yrs::GetString::get_string(&t, &txn);
            }
        }
        "".to_string()
    };

    // Load expected from the first client
    let mut expected_files: HashMap<String, String> = HashMap::new();
    let mut expected_yrs: HashMap<String, String> = HashMap::new();

    for entry in walkdir::WalkDir::new(&client_dirs[0])
        .min_depth(1)
        .max_depth(1)
    {
        let entry = entry?;
        let path = entry.path();

        let path_str = path.to_string_lossy();
        if path_str.contains(".syncline") || path_str.contains(".git") {
            continue;
        }

        if path.is_file() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let content = fs::read_to_string(path)?;
            expected_files.insert(name.clone(), content);
            expected_yrs.insert(name.clone(), load_yrs(&client_dirs[0], &name));
        }
    }

    let mut converged = true;

    for (idx, dir) in client_dirs.iter().enumerate().skip(1) {
        let mut actual_files: HashMap<String, String> = HashMap::new();
        let mut actual_yrs: HashMap<String, String> = HashMap::new();

        for entry in walkdir::WalkDir::new(dir).min_depth(1).max_depth(1) {
            let entry = entry?;
            let path = entry.path();
            let path_str = path.to_string_lossy();
            if path_str.contains(".syncline") || path_str.contains(".git") {
                continue;
            }
            if path.is_file() {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let content = fs::read_to_string(path)?;
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
            if let Some(expected_content) = expected_files.get(name) {
                if content != expected_content {
                    error!(
                        "DISK File {} mismatches between Client 0 and Client {}.\nClient 0: {:?}\nClient {}: {:?}",
                        name, idx, expected_content, idx, content
                    );
                    converged = false;
                }
            }
        }

        for (name, content) in &actual_yrs {
            if let Some(expected_content) = expected_yrs.get(name) {
                if content != expected_content {
                    error!(
                        "YRS File {} mismatches between Client 0 and Client {}.\nClient 0: {:?}\nClient {}: {:?}",
                        name, idx, expected_content, idx, content
                    );
                    converged = false;
                }
            }
        }
    }

    Ok(converged)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let args = Args::parse();
    info!(
        "Starting fuzz test with {} clients, seed {}, duration {}s",
        args.clients, args.seed, args.duration_secs
    );

    build_binaries().await.context("Failed to build binaries")?;

    let binary_dir = std::env::current_dir()?.join("target/debug");
    let server_bin = binary_dir.join("server");
    let client_bin = binary_dir.join("client_folder");

    // Setup working directories
    let server_dir = TempDir::new()?;
    let mut client_dirs = Vec::new();
    for _ in 0..args.clients {
        client_dirs.push(TempDir::new()?);
    }

    // Start server
    let db_path = format!(
        "sqlite://{}?mode=rwc",
        server_dir.path().join("fuzz.db").display()
    );
    info!("Starting Server on port {}", args.port);
    let mut server_child = Command::new(&server_bin)
        .arg("--port")
        .arg(args.port.to_string())
        .arg("--db-path")
        .arg(&db_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn server")?;

    // Give server a moment to bind
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Start clients
    let mut client_children = Vec::new();
    for (i, c_dir) in client_dirs.iter().enumerate() {
        info!("Starting Client {} watching {:?}", i, c_dir.path());
        let child = Command::new(&client_bin)
            .arg(c_dir.path())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context(format!("Failed to spawn client {}", i))?;
        client_children.push(child);
    }

    // Give clients a moment to connect
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let running = Arc::new(AtomicBool::new(true));

    let mut tasks = Vec::new();
    for i in 0..args.clients {
        let path = client_dirs[i].path().to_path_buf();
        let running_clone = running.clone();
        let t = tokio::spawn(runner_loop(i, path, args.seed, running_clone));
        tasks.push(t);
    }

    // Run for duration
    info!(
        "Fuzzing active, waiting for {} seconds...",
        args.duration_secs
    );
    tokio::time::sleep(Duration::from_secs(args.duration_secs)).await;

    // Stop mutations
    info!("Stopping mutations...");
    running.store(false, Ordering::Relaxed);

    for t in tasks {
        let _ = t.await;
    }

    info!("Mutations stopped. Waiting 10 seconds for sync convergence...");
    // Give plenty of time to debounce (300ms) and network sync to complete
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Compare
    info!("Comparing client directories...");
    let paths: Vec<PathBuf> = client_dirs
        .iter()
        .map(|td| td.path().to_path_buf())
        .collect();
    let converged = compare_directories(&paths)?;

    if converged {
        info!(
            "✅ SUCCESS! All clients successfully synchronized to identical states despite conflicts."
        );
    } else {
        error!("❌ FAILURE! Clients diverged.");
    }

    // Cleanup
    info!("Shutting down processes...");
    for mut child in client_children {
        let _ = child.kill().await;
    }
    let _ = server_child.kill().await;

    if !converged {
        anyhow::bail!("Fuzz test failed - states diverged.");
    }

    Ok(())
}
