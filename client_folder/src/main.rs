use crate::client::Client;
use base64::prelude::*;
use clap::Parser;
mod client;

use notify::{Event, RecursiveMode, Result, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use walkdir::WalkDir;
use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, Map, Observable, ReadTxn, Subscription, Text, Transact, Update};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "ws://127.0.0.1:3030/sync")]
    url: String,

    #[arg(short, long)]
    dir: PathBuf,

    /// Path component names to exclude from sync (directory or file names).
    /// If provided, replaces the defaults entirely. .syncline is always excluded regardless.
    #[arg(long, value_name = "NAME", default_values = ["node_modules", ".git", ".obsidian", "target", ".DS_Store"])]
    exclude: Vec<String>,
}

struct SendSubscription(#[allow(dead_code)] Subscription);
unsafe impl Send for SendSubscription {}
unsafe impl Sync for SendSubscription {}

struct ActiveFile {
    doc: Doc,
    file_path: PathBuf,
    txn_lock: Arc<tokio::sync::Mutex<()>>,
    _sub: SendSubscription,
}

struct FileRegistry {
    active: HashMap<String, Arc<ActiveFile>>,
    pending: HashSet<String>,
}

impl FileRegistry {
    fn new() -> Self {
        Self {
            active: HashMap::new(),
            pending: HashSet::new(),
        }
    }

    fn try_claim(&mut self, rel_path: &str) -> bool {
        if self.active.contains_key(rel_path) || self.pending.contains(rel_path) {
            false
        } else {
            self.pending.insert(rel_path.to_string());
            true
        }
    }

    fn activate(&mut self, rel_path: String, handler: Arc<ActiveFile>) {
        self.pending.remove(&rel_path);
        self.active.insert(rel_path, handler);
    }

    fn unclaim(&mut self, rel_path: &str) {
        self.pending.remove(rel_path);
    }

    fn is_active(&self, rel_path: &str) -> bool {
        self.active.contains_key(rel_path)
    }

    fn get_active(&self, rel_path: &str) -> Option<Arc<ActiveFile>> {
        self.active.get(rel_path).cloned()
    }
}

fn meta_dir(root_dir: &Path) -> PathBuf {
    root_dir.join(".syncline")
}

fn crdt_state_path(root_dir: &Path, rel_path: &str) -> PathBuf {
    let safe_name = rel_path.replace(['/', '\\'], "_");
    meta_dir(root_dir).join(format!("{}.yrs", safe_name))
}

fn is_excluded(path: &Path, exclusions: &[String]) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str();
        if s == ".syncline" {
            return true;
        }
        exclusions.iter().any(|e| s == e.as_str())
    })
}

fn persist_doc(root_dir: &Path, rel_path: &str, doc: &Doc) {
    let path = crdt_state_path(root_dir, rel_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let txn = doc.transact();
    let state = txn.encode_state_as_update_v1(&yrs::StateVector::default());
    if let Err(e) = std::fs::write(&path, &state) {
        log::error!("Failed to persist CRDT state for {}: {}", rel_path, e);
    }
}

fn persist_update_incremental(root_dir: &Path, rel_path: &str, update_data: &[u8]) {
    let path = crdt_state_path(root_dir, rel_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let doc = Doc::new();
    if path.exists() {
        if let Ok(existing) = std::fs::read(&path) {
            if let Ok(u) = Update::decode_v1(&existing) {
                let mut txn = doc.transact_mut();
                txn.apply_update(u);
            }
        }
    }
    if let Ok(u) = Update::decode_v1(update_data) {
        let mut txn = doc.transact_mut();
        txn.apply_update(u);
    }
    let txn = doc.transact();
    let merged = txn.encode_state_as_update_v1(&yrs::StateVector::default());
    if let Err(e) = std::fs::write(&path, &merged) {
        log::error!("Failed to persist CRDT state for {}: {}", rel_path, e);
    }
}

fn load_or_create_doc(root_dir: &Path, rel_path: &str) -> Doc {
    let doc = Doc::new();
    let path = crdt_state_path(root_dir, rel_path);
    if path.exists() {
        match std::fs::read(&path) {
            Ok(data) => {
                if let Ok(update) = Update::decode_v1(&data) {
                    let mut txn = doc.transact_mut();
                    txn.apply_update(update);
                    log::info!(
                        "Restored persisted CRDT state for {} ({} bytes)",
                        rel_path,
                        data.len()
                    );
                } else {
                    log::warn!("Failed to decode persisted CRDT state for {}", rel_path);
                }
            }
            Err(e) => {
                log::warn!(
                    "Failed to read persisted CRDT state for {}: {}",
                    rel_path,
                    e
                );
            }
        }
    }
    doc
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    if !args.dir.exists() {
        std::fs::create_dir_all(&args.dir)?;
    }
    let canonical_dir = args.dir.canonicalize()?;

    let _ = std::fs::create_dir_all(meta_dir(&canonical_dir));

    log::info!("Syncing directory: {}", canonical_dir.display());

    let registry: Arc<Mutex<FileRegistry>> = Arc::new(Mutex::new(FileRegistry::new()));

    // Create single client connection
    let client = Arc::new(Client::new(&args.url).await?);

    // 1. Setup Index Document (Root)
    let index_doc = load_or_create_doc(&canonical_dir, "__index__");
    let index_map = index_doc.get_or_insert_map("files");

    // 2. Observe Index for Remote Changes
    let registry_clone = registry.clone();
    let client_clone = client.clone();
    let dir_clone = canonical_dir.clone();

    let _index_sub = {
        let map_clone = index_map.clone();
        index_map.observe(move |txn, event| {
            for key in event.keys(txn).keys() {
                let rel_path = key.to_string();

                if !map_clone.contains_key(txn, &rel_path) {
                    log::info!("Remote deletion detected: {}", rel_path);
                    let file_path = dir_clone.join(&rel_path);
                    if file_path.exists() {
                        if let Err(e) = std::fs::remove_file(&file_path) {
                            log::error!("Failed to delete local file {}: {}", rel_path, e);
                        } else {
                            let yrs_path = crdt_state_path(&dir_clone, &rel_path);
                            if yrs_path.exists() {
                                let _ = std::fs::remove_file(yrs_path);
                            }
                            log::info!("Deleted local file: {}", rel_path);
                            registry_clone.lock().unwrap().unclaim(&rel_path);
                        }
                    }
                } else {
                    let should_start = {
                        let mut reg = registry_clone.lock().unwrap();
                        reg.try_claim(&rel_path)
                    };
                    if should_start {
                        log::info!("Discovered remote file in index: {}", rel_path);
                        let reg = registry_clone.clone();
                        let c = client_clone.clone();
                        let d = dir_clone.clone();
                        let rp = rel_path.clone();
                        tokio::spawn(async move {
                            if let Err(e) = start_file_sync(&c, &d, rp.clone(), &reg).await {
                                log::error!("Error starting file sync for {}: {}", rp, e);
                                reg.lock().unwrap().unclaim(&rp);
                            }
                        });
                    }
                }
            }
        })
    };

    let dir_for_index_persist = canonical_dir.clone();
    let _index_persist_sub = index_doc.observe_update_v1(move |_txn, event| {
        persist_update_incremental(&dir_for_index_persist, "__index__", &event.update);
    });

    // Add index doc to client; capture per-doc transaction lock
    let index_txn_lock = client.add_doc("__index__".to_string(), index_doc.clone()).await?;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    persist_doc(&canonical_dir, "__index__", &index_doc);

    // 3. Scan Local Files
    {
        let mut local_files = Vec::new();
        for entry in WalkDir::new(&canonical_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() {
                if is_excluded(path, &args.exclude) {
                    continue;
                }
                if let Some(ext) = path.extension() {
                    if ext == "md" || ext == "txt" {
                        if let Ok(rel_path) = path.strip_prefix(&canonical_dir) {
                            local_files.push(rel_path.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        if !local_files.is_empty() {
            let _guard = index_txn_lock.lock().await;
            let mut txn = index_doc.transact_mut();
            for f in &local_files {
                index_map.insert(&mut txn, f.clone(), "1");
            }
        }

        for f in local_files {
            let should_start = {
                let mut reg = registry.lock().unwrap();
                reg.try_claim(&f)
            };
            if should_start {
                if let Err(e) =
                    start_file_sync(&client, &canonical_dir, f.clone(), &registry).await
                {
                    log::error!("Error starting file sync for {}: {}", f, e);
                    registry.lock().unwrap().unclaim(&f);
                }
            }
        }
    }

    // Sync remote-only files
    {
        let txn = index_doc.transact();
        let remote_files: Vec<String> = index_map.keys(&txn).map(|k| k.to_string()).collect();
        drop(txn);

        for f in remote_files {
            let should_start = {
                let mut reg = registry.lock().unwrap();
                reg.try_claim(&f)
            };
            if should_start {
                log::info!("Found remote-only file in index: {}", f);
                if let Err(e) =
                    start_file_sync(&client, &canonical_dir, f.clone(), &registry).await
                {
                    log::error!("Error starting file sync for {}: {}", f, e);
                    registry.lock().unwrap().unclaim(&f);
                }
            }
        }
    }

    // 4. Watch for changes
    let (tx, mut rx) = mpsc::channel(100);
    let mut watcher = notify::recommended_watcher(move |res: Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    })?;
    watcher.watch(&canonical_dir, RecursiveMode::Recursive)?;

    log::info!("Watching for changes...");

    while let Some(event) = rx.recv().await {
        for path in event.paths {
            if is_excluded(&path, &args.exclude) {
                continue;
            }

            if let Ok(rel_path) = path.strip_prefix(&canonical_dir) {
                let rel_path_str = rel_path.to_string_lossy().to_string();

                if path.exists() && path.is_file() {
                    {
                        let _guard = index_txn_lock.lock().await;
                        let mut txn = index_doc.transact_mut();
                        let in_index = index_map.contains_key(&txn, &rel_path_str);
                        if !in_index {
                            index_map.insert(&mut txn, rel_path_str.clone(), "1");
                        }
                    }

                    let (is_active, handler) = {
                        let reg = registry.lock().unwrap();
                        let active = reg.is_active(&rel_path_str);
                        let h = reg.get_active(&rel_path_str);
                        (active, h)
                    };

                    if !is_active {
                        let should_start = {
                            let mut reg = registry.lock().unwrap();
                            reg.try_claim(&rel_path_str)
                        };
                        if should_start {
                            if let Err(e) = start_file_sync(
                                &client,
                                &canonical_dir,
                                rel_path_str.clone(),
                                &registry,
                            )
                            .await
                            {
                                log::error!("Error starting file sync: {}", e);
                                registry.lock().unwrap().unclaim(&rel_path_str);
                            }
                        }
                    } else if let Some(h) = handler {
                        if let Err(e) = sync_local_change(&h).await {
                            log::error!("Error syncing local change: {}", e);
                        }
                    }
                } else if !path.exists() {
                    {
                        let _guard = index_txn_lock.lock().await;
                        let mut txn = index_doc.transact_mut();
                        if index_map.contains_key(&txn, &rel_path_str) {
                            index_map.remove(&mut txn, &rel_path_str);
                            log::info!("Removed from index (local delete): {}", rel_path_str);
                        }
                    }

                    registry.lock().unwrap().unclaim(&rel_path_str);
                }
            }
        }
    }

    Ok(())
}

async fn start_file_sync(
    client: &Arc<Client>,
    root_dir: &Path,
    rel_path: String,
    registry: &Arc<Mutex<FileRegistry>>,
) -> anyhow::Result<()> {
    let file_path = root_dir.join(&rel_path);
    let doc_id = rel_path.clone();

    log::info!("Starting sync for file: {} (doc_id: {})", rel_path, doc_id);

    let doc = load_or_create_doc(root_dir, &rel_path);
    let text = doc.get_or_insert_text("content");

    if file_path.exists() {
        if let Ok(file_bytes) = tokio::fs::read(&file_path).await {
            let (_is_binary, local_content) = match String::from_utf8(file_bytes.clone()) {
                Ok(s) => (false, s),
                Err(_) => (
                    true,
                    format!("BINARY:{}", BASE64_STANDARD.encode(&file_bytes)),
                ),
            };

            let current_doc_content = {
                let txn = doc.transact();
                text.get_string(&txn)
            };

            if local_content != current_doc_content {
                if current_doc_content.is_empty() && !local_content.is_empty() {
                    let mut txn = doc.transact_mut();
                    text.insert(&mut txn, 0, &local_content);
                    log::info!(
                        "Inserted local content ({} chars) into doc for {}",
                        local_content.len(),
                        rel_path
                    );
                } else if !local_content.is_empty() {
                    let diffs = diff::chars(&current_doc_content, &local_content);
                    let mut txn = doc.transact_mut();
                    let mut index = 0u32;
                    for d in diffs {
                        match d {
                            diff::Result::Left(_) => {
                                text.remove_range(&mut txn, index, 1);
                            }
                            diff::Result::Right(r) => {
                                let s = r.to_string();
                                text.insert(&mut txn, index, &s);
                                index += 1;
                            }
                            diff::Result::Both(_, _) => {
                                index += 1;
                            }
                        }
                    }
                    log::info!("Applied local offline edits for {}", rel_path);
                }
            }
        }
    }

    persist_doc(root_dir, &rel_path, &doc);

    let file_path_clone = file_path.clone();
    let text_clone = text.clone();
    let root_dir_persist = root_dir.to_path_buf();
    let rel_path_persist = rel_path.clone();
    let sub = SendSubscription(text.observe(move |txn, _event| {
        let content = text_clone.get_string(txn);

        let trimmed = content.trim();
        if trimmed.starts_with("BINARY:") {
            let items: Vec<&str> = trimmed.splitn(2, "BINARY:").collect();
            let b64 = items.get(1).unwrap_or(&"");
            let b64_clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();

            if let Ok(bytes) = BASE64_STANDARD.decode(&b64_clean) {
                if let Ok(current) = std::fs::read(&file_path_clone) {
                    if current == bytes {
                        return;
                    }
                }
                if let Some(parent) = file_path_clone.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&file_path_clone, &bytes) {
                    log::error!("Failed to write binary file in observer: {}", e);
                } else {
                    log::info!(
                        "Observer wrote {} bytes (binary) to {}",
                        bytes.len(),
                        file_path_clone.display()
                    );
                }
            } else {
                log::error!(
                    "Failed to decode base64 binary content for {}:ContentStart:{}",
                    file_path_clone.display(),
                    &trimmed.chars().take(20).collect::<String>()
                );
            }
        } else {
            if let Ok(current) = std::fs::read_to_string(&file_path_clone) {
                if current == content {
                    return;
                }
            }
            if let Some(parent) = file_path_clone.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&file_path_clone, &content) {
                log::error!("Failed to write file in observer: {}", e);
            } else {
                log::info!(
                    "Observer wrote {} chars to {}",
                    content.len(),
                    file_path_clone.display()
                );
            }
        }
        let state = txn.encode_state_as_update_v1(&yrs::StateVector::default());
        let path = crdt_state_path(&root_dir_persist, &rel_path_persist);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, &state) {
            log::error!(
                "Failed to persist CRDT state for {}: {}",
                rel_path_persist,
                e
            );
        }
    }));

    let txn_lock = client.add_doc(doc_id.clone(), doc.clone()).await?;

    let initial_update = {
        let _g = txn_lock.lock().await;
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    };
    if let Err(e) = client.send_update(&doc_id, initial_update).await {
        log::error!(
            "Failed to send initial state to server for {}: {}",
            rel_path,
            e
        );
    }

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    {
        let _g = txn_lock.lock().await;
        persist_doc(root_dir, &rel_path, &doc);
    }

    {
        let _g = txn_lock.lock().await;
        let txn = doc.transact();
        let content = text.get_string(&txn);
        if !content.is_empty() {
            if let Some(parent) = file_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            let trimmed = content.trim();
            if trimmed.starts_with("BINARY:") {
                let items: Vec<&str> = trimmed.splitn(2, "BINARY:").collect();
                let b64 = items.get(1).unwrap_or(&"");
                let b64_clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                if let Ok(bytes) = BASE64_STANDARD.decode(&b64_clean) {
                    let _ = std::fs::write(&file_path, &bytes);
                    log::info!(
                        "Wrote doc content to file (binary): {} ({} bytes)",
                        file_path.display(),
                        bytes.len()
                    );
                } else {
                    log::error!("Failed to decode binary content in safety net");
                }
            } else {
                let _ = std::fs::write(&file_path, &content);
                log::info!(
                    "Wrote doc content to file: {} ({} chars)",
                    file_path.display(),
                    content.len()
                );
            }
        }
    }

    let handler = Arc::new(ActiveFile {
        doc,
        file_path,
        txn_lock,
        _sub: sub,
    });

    if let Err(e) = sync_local_change(&handler).await {
        log::error!("Initial sync_local_change failed for {}: {}", rel_path, e);
    }

    registry.lock().unwrap().activate(rel_path.clone(), handler);
    log::info!("File sync active for: {}", rel_path);

    Ok(())
}

async fn sync_local_change(handler: &ActiveFile) -> anyhow::Result<()> {
    if !handler.file_path.exists() {
        return Ok(());
    }

    let file_bytes = tokio::fs::read(&handler.file_path).await?;
    let (_is_binary, content) = match String::from_utf8(file_bytes.clone()) {
        Ok(s) => (false, s),
        Err(_) => (
            true,
            format!("BINARY:{}", BASE64_STANDARD.encode(&file_bytes)),
        ),
    };
    let text = handler.doc.get_or_insert_text("content");

    let _guard = handler.txn_lock.lock().await;
    let current_y_text = {
        let txn = handler.doc.transact();
        text.get_string(&txn)
    };

    if current_y_text == content {
        return Ok(());
    }

    log::info!(
        "Syncing local change for {}: '{}' -> '{}'",
        handler.file_path.display(),
        &current_y_text[..current_y_text.len().min(50)],
        &content[..content.len().min(50)]
    );

    let diffs = diff::chars(&current_y_text, &content);
    let mut txn = handler.doc.transact_mut();
    let mut index = 0u32;

    for d in diffs {
        match d {
            diff::Result::Left(_) => {
                text.remove_range(&mut txn, index, 1);
            }
            diff::Result::Right(r) => {
                let s = r.to_string();
                text.insert(&mut txn, index, &s);
                index += 1;
            }
            diff::Result::Both(_, _) => {
                index += 1;
            }
        }
    }

    Ok(())
}