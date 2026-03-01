use crate::client::diff::apply_diff_to_yrs;
use crate::client::network::SynclineClient;
use crate::client::protocol::{Message, MsgType};
use crate::client::state::LocalState;
use crate::client::storage::{load_doc, save_doc};
use crate::client::watcher::DebouncedWatcher;
use colored::Colorize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

pub async fn run_client(folder: PathBuf, url: String, name: Option<String>) -> anyhow::Result<()> {
    println!(
        "{}",
        r#"
   _____                  ___
  / ___/__  ______  _____/ (_)___  ___
  \__ \/ / / / __ \/ ___/ / / __ \/ _ \
 ___/ / /_/ / / / / /__/ / / / / /  __/
/____/\__, /_/ /_/\___/_/_/_/ /_/\___/
     /____/
"#
        .cyan()
        .bold()
    );
    println!("  {}\n", "🌟 A modern synchronization workspace".green());

    info!("{} Starting Syncline Client...", "🚀".green());
    info!("{} Folder: {}", "📂".blue(), folder.display());
    info!("{} Server URL: {}", "🌐".cyan(), url);

    let dir_to_watch = folder;
    let local_state = LocalState::new(&dir_to_watch, name);
    info!("Client name: {}", local_state.client_name);

    // Bootstrap all local documents BEFORE connecting to the server.
    // This ensures local state is fully consistent (disk → CRDT) before any
    // network activity begins. The `freshly_created` set tracks doc_ids that
    // had no prior `.bin` state — used later for conflict detection.
    let (initial_offline_changes, mut freshly_created) =
        match local_state.bootstrap_offline_changes() {
            Ok(result) => result,
            Err(e) => {
                error!("Error bootstrapping offline changes: {:?}", e);
                (Vec::new(), HashSet::new())
            }
        };
    // Use take() so offline changes are broadcast exactly once (on first
    // successful connection). On subsequent reconnects this is None.
    let mut pending_offline_changes: Option<Vec<(String, Vec<u8>)>> =
        Some(initial_offline_changes);

    // Setup network
    let server_url = url;
    let mut client = SynclineClient::new(&server_url)?;

    // Setup debounced watcher (created outside reconnect loop)
    let (watcher_tx, mut watcher_rx) = mpsc::channel(100);
    let mut watcher = DebouncedWatcher::new(watcher_tx, std::time::Duration::from_millis(300))?;
    watcher.watch(&dir_to_watch)?;

    info!("Listening for file events. Press Ctrl+C to stop.");

    let mut reconnect_attempts = 0;

    // Outer loop for reconnecting
    loop {
        let (app_tx, mut app_rx) = mpsc::channel(100);
        let ws_tx = match client.connect(app_tx).await {
            Ok(tx) => {
                reconnect_attempts = 0;
                tx
            }
            Err(e) => {
                let delay = std::cmp::min(1000 * 2u64.pow(reconnect_attempts), 30000);
                error!("Connection failed: {:?}. Retrying in {}ms", e, delay);
                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                reconnect_attempts += 1;
                continue;
            }
        };

        // Track which doc_ids we have subscribed to via SyncStep1 so we can
        // subscribe on-the-fly when the watcher discovers a new file.
        let mut subscribed_docs: HashSet<String> = HashSet::new();

        // Phase 4: send MSG_SYNC_STEP_1 for __index__ and all known documents
        if let Ok(docs) = local_state.list_doc_ids() {
            for doc_id in docs {
                let state_path = local_state.get_state_path(&doc_id);
                if let Ok(doc) = load_doc(&state_path) {
                    let sv = doc.transact().state_vector().encode_v1();
                    if let Err(e) = ws_tx
                        .send(Message::new(MsgType::SyncStep1, doc_id.clone(), sv))
                        .await
                    {
                        error!("Failed to send SyncStep1 for doc {}: {:?}", doc_id, e);
                    }
                    subscribed_docs.insert(doc_id);
                }
            }
        }

        // Request sync for __index__
        if let Err(e) = ws_tx
            .send(Message::new(
                MsgType::SyncStep1,
                "__index__".to_string(),
                StateVector::default().encode_v1(),
            ))
            .await
        {
            error!("Failed to send SyncStep1 for __index__: {:?}", e);
        }
        subscribed_docs.insert("__index__".to_string());

        // Broadcast offline changes (first connection only; None on reconnects)
        let offline_changes = pending_offline_changes.take().unwrap_or_default();
        for (doc_id, update) in offline_changes {
            if let Err(e) = ws_tx
                .send(Message::new(MsgType::Update, doc_id.clone(), update))
                .await
            {
                error!("Failed to broadcast offline update for {}: {:?}", doc_id, e);
            } else {
                info!("Broadcasted offline changes for {}", doc_id);
            }
        }

        loop {
            tokio::select! {
                    app_msg = app_rx.recv() => {
                        let msg = match app_msg {
                            Some(m) => m,
                            None => {
                                error!("Connection to server lost. Reconnecting...");
                                break; // break the inner loop to trigger reconnect
                            }
                        };
                        match msg.msg_type {
                        MsgType::SyncStep2 | MsgType::Update => {
                            let doc_id = msg.doc_id;
                            let is_index = doc_id == "__index__";

                            let state_path = local_state.get_state_path(&doc_id);
                            let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());

                            if is_index {
                                info!("Received update for __index__, msg payload len: {}", msg.payload.len());
                                match Update::decode_v1(&msg.payload) {
                                    Ok(update) => {
                                        let text_ref = doc.get_or_insert_text("content");
                                        let mut txn = doc.transact_mut();
                                        txn.apply_update(update);

                                        let index_content = text_ref.get_string(&txn);
                                        drop(txn);

                                        info!("__index__ content is now: {:?}", index_content);

                                        if let Err(e) = save_doc(&doc, &state_path) {
                                            error!("Failed to save doc state {}: {:?}", doc_id, e);
                                        }

                                        let new_index_docs: HashSet<&str> = index_content.lines().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();

                                        // Detect deletions: docs that are in subscribed_docs but no longer in __index__
                                        let mut to_remove = Vec::new();
                                        for sub_doc in &subscribed_docs {
                                            if sub_doc == "__index__" { continue; }
                                            if !new_index_docs.contains(sub_doc.as_str()) {
                                                to_remove.push(sub_doc.clone());
                                            }
                                        }

                                        for removed_doc in to_remove {
                                            info!("Document {} removed from __index__, deleting locally", removed_doc);
                                            let phys_path = local_state.root_dir.join(&removed_doc);
                                            if fs::metadata(&phys_path).is_ok() {
                                                if let Err(e) = fs::remove_file(&phys_path) {
                                                    error!("Failed to delete physical file {}: {:?}", phys_path.display(), e);
                                                } else {
                                                    info!("Deleted physical file {}", phys_path.display());
                                                }
                                            }
                                            subscribed_docs.remove(&removed_doc);
                                        }

                                        for discovered_doc_id in new_index_docs {
                                            if !subscribed_docs.contains(discovered_doc_id) {
                                                info!("Discovered new document from __index__: {}", discovered_doc_id);
                                                let discovered_state_path = local_state.get_state_path(discovered_doc_id);
                                                info!("Sending SyncStep1 for newly discovered doc: {}", discovered_doc_id);
                                                let discovered_doc = load_doc(&discovered_state_path).unwrap_or_else(|_| Doc::new());
                                                let sv = discovered_doc.transact().state_vector().encode_v1();

                                                if let Err(e) = ws_tx.send(Message::new(
                                                    MsgType::SyncStep1,
                                                    discovered_doc_id.to_string(),
                                                    sv,
                                                )).await {
                                                    error!("Failed to send SyncStep1 for discovered doc {}: {:?}", discovered_doc_id, e);
                                                } else {
                                                    info!("Successfully sent SyncStep1 for {}!", discovered_doc_id);
                                                    subscribed_docs.insert(discovered_doc_id.to_string());
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to decode __index__ update: {:?}", e);
                                    }
                                }
                                continue;
                            }

                            // --- Conflict detection for freshly created docs ---
                            // A freshly created doc is one that had no .bin state before bootstrap,
                            // meaning the client independently authored it. If the server also has
                            // content for the same doc_id, the two documents have no shared CRDT
                            // history and a naive merge would produce garbled output. Instead, we
                            // keep both copies: server content as the canonical file, local content
                            // renamed with the client name as a suffix.
                            if freshly_created.contains(&doc_id) {
                                freshly_created.remove(&doc_id);

                                if !msg.payload.is_empty() {
                                    if let Ok(server_update) = Update::decode_v1(&msg.payload) {
                                        let server_doc = Doc::new();
                                        let server_text_ref = server_doc.get_or_insert_text("content");
                                        {
                                            let mut txn = server_doc.transact_mut();
                                            txn.apply_update(server_update);
                                        }
                                        let server_text = server_text_ref.get_string(&server_doc.transact());

                                        let local_text_ref = doc.get_or_insert_text("content");
                                        let local_text = local_text_ref.get_string(&doc.transact());

                                        if !server_text.is_empty()
                                            && !local_text.is_empty()
                                            && server_text != local_text
                                        {
                                            info!(
                                                "Creation conflict detected for '{}': local content differs from server. Resolving...",
                                                doc_id
                                            );
                                            if let Err(e) = resolve_creation_conflict(
                                                &local_state,
                                                &doc_id,
                                                &local_text,
                                                &server_text,
                                                &server_doc,
                                                &ws_tx,
                                                &mut subscribed_docs,
                                            ).await {
                                                error!("Conflict resolution failed for {}: {:?}", doc_id, e);
                                            }
                                            continue; // skip normal SyncStep2 processing
                                        }
                                        // No true conflict (e.g. server has same content or empty) —
                                        // fall through to normal processing below.
                                    }
                                }
                                // payload empty or not decodable — no server content yet, fall through
                            }
                            // --- End conflict detection ---

                            let phys_path = local_state.root_dir.join(&doc_id);

                            // 1. Check for unsynced local disk changes before we overwrite the disk
                            let text_ref = doc.get_or_insert_text("content");
                            let yjs_content_before = text_ref.get_string(&doc.transact());
                            let has_file = fs::metadata(&phys_path).is_ok();
                            let disk_content = if has_file {
                                match fs::read_to_string(&phys_path) {
                                    Ok(content) => content,
                                    Err(e) => {
                                        error!("Failed to read physical file {}: {:?}", phys_path.display(), e);
                                        continue;
                                    }
                                }
                            } else {
                                String::new()
                            };

                            if has_file && yjs_content_before != disk_content {
                                info!("Unsynced local changes in {}! Diffing before applying remote update.", doc_id);
                                let previous_sv = doc.transact().state_vector();
                                apply_diff_to_yrs(&doc, &text_ref, &yjs_content_before, &disk_content);

                                // Broadcast the local edits so the other remote peers get them too
                                let local_update = doc.transact().encode_state_as_update_v1(&previous_sv);
                                if let Err(e) = ws_tx.send(Message::new(MsgType::Update, doc_id.clone(), local_update)).await {
                                    error!("Failed to send unsynced local update: {:?}", e);
                                }
                            }

                            // 2. NOW apply the remote update
                            if let Ok(update) = Update::decode_v1(&msg.payload) {
                                let mut txn = doc.transact_mut();
                                txn.apply_update(update);

                                let text_val = text_ref.get_string(&txn);
                                drop(txn);

                                if let Err(e) = save_doc(&doc, &state_path) {
                                    error!("Failed to save doc state {}: {:?}", doc_id, e);
                                }

                                let current_disk_content = fs::read_to_string(&phys_path).unwrap_or_default();
                                let file_exists = fs::metadata(&phys_path).is_ok();

                                if text_val.is_empty() {
                                    if file_exists {
                                        if let Err(e) = fs::remove_file(&phys_path) {
                                            error!("Failed to delete physical file {}: {:?}", phys_path.display(), e);
                                        } else {
                                            info!("Applied remote deletion to file {}", phys_path.display());
                                        }
                                    }
                                } else if !file_exists || current_disk_content != text_val {
                                    if let Some(parent) = phys_path.parent() {
                                        let _ = fs::create_dir_all(parent);
                                    }

                                    if let Err(e) = fs::write(&phys_path, &text_val) {
                                        error!("Failed to write physical file {}: {:?}", phys_path.display(), e);
                                    } else {
                                        info!("Applied remote update to file {}", phys_path.display());
                                    }
                                }
                            }
                        },
                        MsgType::SyncStep1 => { }
                    }
                }
                Some(res) = watcher_rx.recv() => {
                    match res {
                        Ok(events) => {
                            for ev in events {
                                let path = ev.path;

                                let is_deleted = !path.exists();
                                if is_deleted {
                                    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                                    if ext != "md" && ext != "txt" { continue; }

                                    let doc_id = match local_state.get_doc_id(&path) {
                                        Ok(d) => d,
                                        Err(_) => continue,
                                    };

                                    let state_path = local_state.get_state_path(&doc_id);
                                    if !state_path.exists() {
                                        continue; // Never tracked, ignore deletion
                                    }
                                } else if !path.is_file() {
                                    continue;
                                } else {
                                    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                                    if ext != "md" && ext != "txt" { continue; }
                                }

                                let doc_id = match local_state.get_doc_id(&path) {
                                    Ok(d) => d,
                                    Err(_) => continue,
                                };

                                // Check if any part of the relative path is hidden (starts with .)
                                if doc_id.split('/').any(|comp| comp.starts_with('.')) {
                                    continue;
                                }
                                let state_path = local_state.get_state_path(&doc_id);

                                let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());
                                let text_ref = doc.get_or_insert_text("content");

                                let newly_discovered = !subscribed_docs.contains(&doc_id);
                                // If this document has never been subscribed to, send SyncStep1
                                // first so the server creates a broadcast channel for it and we
                                // receive updates from other clients.
                                if newly_discovered {
                                    let sv = doc.transact().state_vector().encode_v1();
                                    if let Err(e) = ws_tx
                                        .send(Message::new(MsgType::SyncStep1, doc_id.clone(), sv))
                                        .await
                                    {
                                        error!("Failed to subscribe to new doc {}: {:?}", doc_id, e);
                                    } else {
                                        subscribed_docs.insert(doc_id.clone());
                                    }
                                }

                                let disk_content = if is_deleted {
                                    "".to_string()
                                } else {
                                    match fs::read_to_string(&path) {
                                        Ok(c) => c,
                                        Err(_) => continue,
                                    }
                                };

                                let yjs_content = text_ref.get_string(&doc.transact());
                                if newly_discovered || disk_content != yjs_content {
                                    info!("Local file {} modified or newly discovered, diffing and broadcasting...", path.display());
                                    let previous_sv = doc.transact().state_vector();
                                    apply_diff_to_yrs(&doc, &text_ref, &yjs_content, &disk_content);
                                    if let Err(e) = save_doc(&doc, &state_path) {
                                        error!("Failed to save doc locally: {:?}", e);
                                        continue;
                                    }

                                    let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                                    if let Err(e) = ws_tx.send(Message::new(MsgType::Update, doc_id, update)).await {
                                        error!("Failed to send update: {:?}", e);
                                    }
                                }
                            }
                        }
                        Err(e) => error!("Watcher error: {:?}", e),
                    }
                }
            }
        }
    }
}

/// Generates a conflict filename by appending the client name to the stem.
///
/// For `doc_id = "notes/idea.md"` and `client_name = "laptop-abc123"`:
/// - attempt 0 → `"notes/idea (laptop-abc123).md"`
/// - attempt 1 → `"notes/idea (laptop-abc123 2).md"`
fn make_conflict_doc_id(doc_id: &str, client_name: &str, attempt: u32) -> String {
    let path = std::path::Path::new(doc_id);
    let parent = path
        .parent()
        .map(|p| {
            let s = p.to_string_lossy();
            if s.is_empty() { String::new() } else { format!("{}/", s) }
        })
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(doc_id);
    let ext = path.extension().and_then(|s| s.to_str());

    let suffix = if attempt == 0 {
        client_name.to_string()
    } else {
        format!("{} {}", client_name, attempt + 1)
    };

    match ext {
        Some(e) => format!("{}{} ({}).{}", parent, stem, suffix, e),
        None => format!("{}{} ({})", parent, stem, suffix),
    }
}

/// Resolves a creation conflict: two clients independently created a file with the same name.
///
/// The server's content is kept as the canonical file. The local content is saved under
/// a new name `"{stem} ({client_name}).{ext}"` (with a counter suffix to avoid collisions).
/// Both files are synced to the server.
async fn resolve_creation_conflict(
    state: &LocalState,
    doc_id: &str,
    local_text: &str,
    server_text: &str,
    server_doc: &Doc,
    ws_tx: &mpsc::Sender<Message>,
    subscribed_docs: &mut HashSet<String>,
) -> anyhow::Result<()> {
    // Find a conflict filename that doesn't already exist on disk
    let conflict_doc_id = (0u32..)
        .map(|attempt| make_conflict_doc_id(doc_id, &state.client_name, attempt))
        .find(|candidate| !state.root_dir.join(candidate).exists())
        .expect("conflict filename search is unbounded");

    let conflict_phys = state.root_dir.join(&conflict_doc_id);
    let conflict_state_path = state.get_state_path(&conflict_doc_id);

    // Create parent directories for the conflict file if needed
    if let Some(parent) = conflict_phys.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = conflict_state_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write the local content to the conflict file (physical + CRDT)
    fs::write(&conflict_phys, local_text)?;
    let conflict_doc = Doc::new();
    let conflict_text_ref = conflict_doc.get_or_insert_text("content");
    {
        let mut txn = conflict_doc.transact_mut();
        conflict_text_ref.insert(&mut txn, 0, local_text);
    }
    save_doc(&conflict_doc, &conflict_state_path)?;

    // Replace the canonical doc's .bin with the server-only state (no merge)
    let canonical_state_path = state.get_state_path(doc_id);
    save_doc(server_doc, &canonical_state_path)?;

    // Write the server content to the canonical physical file
    let canonical_phys = state.root_dir.join(doc_id);
    if let Some(parent) = canonical_phys.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&canonical_phys, server_text)?;

    info!(
        "Conflict resolved: '{}' keeps server content; local content saved as '{}'",
        doc_id, conflict_doc_id
    );

    // Subscribe to the conflict file on the server, then upload its content
    let sv = StateVector::default().encode_v1();
    if let Err(e) = ws_tx
        .send(Message::new(
            MsgType::SyncStep1,
            conflict_doc_id.clone(),
            sv,
        ))
        .await
    {
        warn!("Failed to subscribe to conflict file {}: {:?}", conflict_doc_id, e);
    } else {
        subscribed_docs.insert(conflict_doc_id.clone());
    }

    let conflict_update = conflict_doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    if let Err(e) = ws_tx
        .send(Message::new(
            MsgType::Update,
            conflict_doc_id.clone(),
            conflict_update,
        ))
        .await
    {
        warn!("Failed to upload conflict file {}: {:?}", conflict_doc_id, e);
    }

    Ok(())
}
