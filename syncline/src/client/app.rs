use crate::client::diff::apply_diff_to_yrs;
use crate::client::network::SynclineClient;
use crate::client::protocol::{Message, MsgType};
use crate::client::state::{read_meta_path, write_meta_path, LocalState};
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
use yrs::{Doc, GetString, ReadTxn, StateVector, Transact, Update};

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
    let mut local_state = LocalState::new(&dir_to_watch, name);
    info!("Client name: {}", local_state.client_name);

    // Bootstrap all local documents BEFORE connecting to the server.
    // This ensures local state is fully consistent (disk → CRDT) before any
    // network activity begins. The `freshly_created` set tracks UUIDs that
    // had no prior `.bin` state — used later for path-collision conflict detection.
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

        // Track which UUIDs we have subscribed to via SyncStep1.
        let mut subscribed_docs: HashSet<String> = HashSet::new();
        // Snapshot of the server's __index__ at first connection. Used to prevent
        // false conflict detection: only UUIDs that were on the server BEFORE we
        // connected can be "server truth" vs our freshly-created docs. UUIDs that
        // arrive later (uploaded by other clients after we connected) should be
        // handled by those clients, not by us.
        let mut initial_server_uuids: Option<HashSet<String>> = None;

        // Phase 4: send MSG_SYNC_STEP_1 for __index__ and all known documents
        if let Ok(uuids) = local_state.list_doc_ids() {
            for uuid in uuids {
                let state_path = local_state.get_state_path_for_uuid(&uuid);
                if let Ok(doc) = load_doc(&state_path) {
                    let sv = doc.transact().state_vector().encode_v1();
                    if let Err(e) = ws_tx
                        .send(Message::new(MsgType::SyncStep1, uuid.clone(), sv))
                        .await
                    {
                        error!("Failed to send SyncStep1 for uuid {}: {:?}", uuid, e);
                    }
                    subscribed_docs.insert(uuid);
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
        for (uuid, update) in offline_changes {
            if let Err(e) = ws_tx
                .send(Message::new(MsgType::Update, uuid.clone(), update))
                .await
            {
                error!("Failed to broadcast offline update for {}: {:?}", uuid, e);
            } else {
                info!("Broadcasted offline changes for {}", uuid);
            }
        }

        loop {
            tokio::select! {
                app_msg = app_rx.recv() => {
                    let msg = match app_msg {
                        Some(m) => m,
                        None => {
                            error!("Connection to server lost. Reconnecting...");
                            break;
                        }
                    };
                    match msg.msg_type {
                        MsgType::SyncStep2 | MsgType::Update => {
                            let doc_id = msg.doc_id; // This is now a UUID (or "__index__")
                            let is_index = doc_id == "__index__";

                            let state_path = if is_index {
                                local_state.get_state_path_for_uuid("__index__")
                            } else {
                                local_state.get_state_path_for_uuid(&doc_id)
                            };
                            let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());

                            if is_index {
                                info!("Received update for __index__, payload len: {}", msg.payload.len());
                                match Update::decode_v1(&msg.payload) {
                                    Ok(update) => {
                                        let text_ref = doc.get_or_insert_text("content");
                                        let mut txn = doc.transact_mut();
                                        txn.apply_update(update);

                                        let index_content = text_ref.get_string(&txn);
                                        drop(txn);

                                        info!("__index__ content: {:?}", index_content);

                                        if let Err(e) = save_doc(&doc, &state_path) {
                                            error!("Failed to save __index__ state: {:?}", e);
                                        }

                                        // __index__ contains UUID keys
                                        let new_index_uuids: HashSet<&str> = index_content
                                            .lines()
                                            .map(|s| s.trim())
                                            .filter(|s| !s.is_empty())
                                            .collect();

                                        // Record the server-side UUIDs at first connection.
                                        // This snapshot is used by conflict detection below to
                                        // distinguish "server had this before I connected" (conflict)
                                        // from "another client added this after I connected" (no conflict).
                                        if initial_server_uuids.is_none() {
                                            initial_server_uuids = Some(
                                                new_index_uuids.iter().map(|s| s.to_string()).collect()
                                            );
                                        }

                                        // Detect UUID removals from index → delete local files
                                        let mut to_remove = Vec::new();
                                        for sub_uuid in &subscribed_docs {
                                            if sub_uuid == "__index__" { continue; }
                                            if !new_index_uuids.contains(sub_uuid.as_str()) {
                                                to_remove.push(sub_uuid.clone());
                                            }
                                        }
                                        for removed_uuid in to_remove {
                                            info!("UUID {} removed from __index__, deleting locally", removed_uuid);
                                            // Find the physical path via path_map
                                            if let Some(rel_path) = local_state.path_map.get_path_for_uuid(&removed_uuid).map(|s| s.to_string()) {
                                                let phys_path = local_state.root_dir.join(&rel_path);
                                                if fs::metadata(&phys_path).is_ok() {
                                                    if let Err(e) = fs::remove_file(&phys_path) {
                                                        error!("Failed to delete file {:?}: {:?}", phys_path, e);
                                                    } else {
                                                        info!("Deleted file {}", phys_path.display());
                                                    }
                                                }
                                                local_state.path_map.remove_by_path(&rel_path);
                                                if let Err(e) = local_state.path_map.save() {
                                                    error!("Failed to save path_map: {:?}", e);
                                                }
                                            }
                                            subscribed_docs.remove(&removed_uuid);
                                        }

                                        // Discover new UUIDs in the index
                                        for discovered_uuid in new_index_uuids {
                                            if !subscribed_docs.contains(discovered_uuid) {
                                                info!("Discovered new UUID from __index__: {}", discovered_uuid);
                                                let disc_state_path = local_state.get_state_path_for_uuid(discovered_uuid);
                                                let disc_doc = load_doc(&disc_state_path).unwrap_or_else(|_| Doc::new());
                                                let sv = disc_doc.transact().state_vector().encode_v1();
                                                if let Err(e) = ws_tx.send(Message::new(
                                                    MsgType::SyncStep1,
                                                    discovered_uuid.to_string(),
                                                    sv,
                                                )).await {
                                                    error!("Failed to send SyncStep1 for discovered UUID {}: {:?}", discovered_uuid, e);
                                                } else {
                                                    subscribed_docs.insert(discovered_uuid.to_string());
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

                            // --- Content document handling (UUID-based) ---

                            // 1. Capture current state for local change detection
                            let prev_rel_path = local_state.path_map.get_path_for_uuid(&doc_id).map(|s| s.to_string());
                            let text_ref = doc.get_or_insert_text("content");
                            let yjs_content_before = text_ref.get_string(&doc.transact());

                            // Check for unsynced local disk changes before applying remote update
                            if let Some(ref rp) = prev_rel_path {
                                let phys_path = local_state.root_dir.join(rp);
                                if let Ok(disk_content) = fs::read_to_string(&phys_path) {
                                    if disk_content != yjs_content_before {
                                        info!("Unsynced local changes in {}! Applying before remote update.", rp);
                                        let prev_sv = doc.transact().state_vector();
                                        apply_diff_to_yrs(&doc, &text_ref, &yjs_content_before, &disk_content);
                                        let local_update = doc.transact().encode_state_as_update_v1(&prev_sv);
                                        if let Err(e) = ws_tx.send(Message::new(MsgType::Update, doc_id.clone(), local_update)).await {
                                            error!("Failed to send unsynced local update: {:?}", e);
                                        }
                                    }
                                }
                            }

                            // 2. Apply the remote update
                            if let Ok(update) = Update::decode_v1(&msg.payload) {
                                let mut txn = doc.transact_mut();
                                txn.apply_update(update);
                            }

                            // 3. Read meta.path (the file path this UUID corresponds to)
                            let new_meta_path = read_meta_path(&doc);
                            let text_val = text_ref.get_string(&doc.transact());

                            // Save updated .bin
                            if let Err(e) = save_doc(&doc, &state_path) {
                                error!("Failed to save doc state for {}: {:?}", doc_id, e);
                            }

                            let target_rel_path = match new_meta_path {
                                Some(p) if !p.is_empty() => p,
                                _ => {
                                    // meta.path not set yet; skip writing to disk
                                    info!("UUID {} has no meta.path yet, skipping disk write", doc_id);
                                    continue;
                                }
                            };

                            // 4. Check for path collision with a freshly-created local doc
                            {
                                let collision_local_uuid = local_state.path_map
                                    .get_uuid(&target_rel_path)
                                    .and_then(|u| {
                                        // Only conflict-detect if:
                                        // - Different UUID from ours
                                        // - Our local UUID was freshly created (no prior .bin)
                                        // - The incoming UUID was on the server BEFORE we
                                        //   connected (initial snapshot). UUIDs that appeared
                                        //   after we connected are handled by the other client.
                                        if u != doc_id
                                            && freshly_created.contains(u)
                                            && initial_server_uuids
                                                .as_ref()
                                                .map_or(false, |s| s.contains(&doc_id))
                                        {
                                            Some(u.to_string())
                                        } else {
                                            None
                                        }
                                    });

                                if let Some(local_uuid) = collision_local_uuid {
                                    freshly_created.remove(&local_uuid);
                                    info!(
                                        "Path collision: UUID {} (server) and UUID {} (local, freshly created) both want '{}'. Resolving...",
                                        doc_id, local_uuid, target_rel_path
                                    );
                                    if let Err(e) = resolve_path_conflict(
                                        &mut local_state,
                                        &doc_id,
                                        &target_rel_path,
                                        &local_uuid,
                                        &doc,
                                        &ws_tx,
                                        &mut subscribed_docs,
                                    ).await {
                                        error!("Conflict resolution failed: {:?}", e);
                                    }
                                    continue;
                                }
                            }

                            // 5. Handle path change (rename propagated from remote)
                            if prev_rel_path.as_deref() != Some(target_rel_path.as_str()) {
                                if let Some(ref old_rp) = prev_rel_path {
                                    let old_phys = local_state.root_dir.join(old_rp);
                                    let new_phys = local_state.root_dir.join(&target_rel_path);
                                    if old_phys.exists() {
                                        if let Some(parent) = new_phys.parent() {
                                            let _ = fs::create_dir_all(parent);
                                        }
                                        if let Err(e) = fs::rename(&old_phys, &new_phys) {
                                            error!("Failed to rename {:?} → {:?}: {:?}", old_phys, new_phys, e);
                                        } else {
                                            info!("Renamed {} → {} from remote update", old_rp, target_rel_path);
                                        }
                                    }
                                    local_state.path_map.remove_by_path(old_rp);
                                }
                                local_state.path_map.insert(target_rel_path.clone(), doc_id.clone());
                                if let Err(e) = local_state.path_map.save() {
                                    error!("Failed to save path_map: {:?}", e);
                                }
                            }

                            // 6. Write content to physical file
                            let phys_path = local_state.root_dir.join(&target_rel_path);
                            if text_val.is_empty() {
                                if phys_path.exists() {
                                    if let Err(e) = fs::remove_file(&phys_path) {
                                        error!("Failed to delete file {:?}: {:?}", phys_path, e);
                                    } else {
                                        info!("Deleted file {} (empty content)", phys_path.display());
                                    }
                                }
                            } else {
                                if let Some(parent) = phys_path.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }
                                let current_disk = fs::read_to_string(&phys_path).unwrap_or_default();
                                if current_disk != text_val {
                                    if let Err(e) = fs::write(&phys_path, &text_val) {
                                        error!("Failed to write file {:?}: {:?}", phys_path, e);
                                    } else {
                                        info!("Applied remote update to {}", phys_path.display());
                                    }
                                }
                            }
                        }
                        MsgType::SyncStep1 => {}
                    }
                }

                Some(res) = watcher_rx.recv() => {
                    match res {
                        Ok(events) => {
                            // Two-pass rename detection within the event batch.
                            // First pass: classify events into deletes and creates/modifies.
                            struct WatchDelete {
                                rel_path: String,
                                uuid: String,
                            }
                            struct WatchCreate {
                                path: std::path::PathBuf,
                                rel_path: String,
                                disk_content: String,
                            }

                            let mut batch_deletes: Vec<WatchDelete> = Vec::new();
                            let mut batch_creates: Vec<WatchCreate> = Vec::new();

                            for ev in &events {
                                let path = &ev.path;
                                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                                if ext != "md" && ext != "txt" { continue; }

                                let rel_path = match local_state.get_doc_id(path) {
                                    Ok(r) => r,
                                    Err(_) => continue,
                                };
                                if rel_path.split('/').any(|c| c.starts_with('.')) { continue; }

                                if !path.exists() {
                                    // Delete event
                                    if let Some(uuid) = local_state.path_map.get_uuid(&rel_path) {
                                        let state_path = local_state.get_state_path_for_uuid(uuid);
                                        if state_path.exists() {
                                            batch_deletes.push(WatchDelete {
                                                rel_path,
                                                uuid: uuid.to_string(),
                                            });
                                        }
                                    }
                                } else if path.is_file() {
                                    match fs::read_to_string(path) {
                                        Ok(content) => batch_creates.push(WatchCreate {
                                            path: path.clone(),
                                            rel_path,
                                            disk_content: content,
                                        }),
                                        Err(_) => {}
                                    }
                                }
                            }

                            // Second pass: match deletes with creates (rename detection)
                            let mut matched_renames: Vec<(String, String, String, String)> = Vec::new(); // (uuid, new_rel_path, disk_content, old_rel_path)
                            let mut used_create_indices: HashSet<usize> = HashSet::new();

                            for wd in &batch_deletes {
                                let state_path = local_state.get_state_path_for_uuid(&wd.uuid);
                                let old_content = load_doc(&state_path)
                                    .map(|d| {
                                        let t = d.get_or_insert_text("content");
                                        t.get_string(&d.transact())
                                    })
                                    .unwrap_or_default();

                                for (i, wc) in batch_creates.iter().enumerate() {
                                    if !used_create_indices.contains(&i) && wc.disk_content == old_content {
                                        matched_renames.push((
                                            wd.uuid.clone(),
                                            wc.rel_path.clone(),
                                            wc.disk_content.clone(),
                                            wd.rel_path.clone(),
                                        ));
                                        used_create_indices.insert(i);
                                        break;
                                    }
                                }
                            }

                            let matched_delete_uuids: HashSet<String> = matched_renames.iter().map(|(u, _, _, _)| u.clone()).collect();

                            // Process renames
                            for (uuid, new_rel_path, disk_content, old_rel_path) in matched_renames {
                                info!("Detected rename: {} → {} (UUID {})", old_rel_path, new_rel_path, uuid);
                                local_state.path_map.remove_by_path(&old_rel_path);
                                local_state.path_map.insert(new_rel_path.clone(), uuid.clone());
                                if let Err(e) = local_state.path_map.save() {
                                    error!("Failed to save path_map: {:?}", e);
                                }

                                let state_path = local_state.get_state_path_for_uuid(&uuid);
                                let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());
                                let text_ref = doc.get_or_insert_text("content");
                                let yjs_content = text_ref.get_string(&doc.transact());
                                let previous_sv = doc.transact().state_vector();
                                // Update meta.path and content (content is same, but record for CRDT)
                                write_meta_path(&doc, &new_rel_path);
                                if disk_content != yjs_content {
                                    apply_diff_to_yrs(&doc, &text_ref, &yjs_content, &disk_content);
                                }
                                if let Err(e) = save_doc(&doc, &state_path) {
                                    error!("Failed to save renamed doc {}: {:?}", uuid, e);
                                    continue;
                                }
                                if !subscribed_docs.contains(&uuid) {
                                    let sv = doc.transact().state_vector().encode_v1();
                                    if let Err(e) = ws_tx.send(Message::new(MsgType::SyncStep1, uuid.clone(), sv)).await {
                                        error!("Failed to subscribe to renamed doc {}: {:?}", uuid, e);
                                    } else {
                                        subscribed_docs.insert(uuid.clone());
                                    }
                                }
                                let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                                if let Err(e) = ws_tx.send(Message::new(MsgType::Update, uuid, update)).await {
                                    error!("Failed to send rename update: {:?}", e);
                                }
                            }

                            // Process unmatched deletes (true deletions)
                            for wd in batch_deletes.iter().filter(|wd| !matched_delete_uuids.contains(&wd.uuid)) {
                                info!("File deleted: {} (UUID {})", wd.rel_path, wd.uuid);
                                let state_path = local_state.get_state_path_for_uuid(&wd.uuid);
                                if let Ok(doc) = load_doc(&state_path) {
                                    let text_ref = doc.get_or_insert_text("content");
                                    let yjs_content = text_ref.get_string(&doc.transact());
                                    if !yjs_content.is_empty() {
                                        let previous_sv = doc.transact().state_vector();
                                        apply_diff_to_yrs(&doc, &text_ref, &yjs_content, "");
                                        if let Err(e) = save_doc(&doc, &state_path) {
                                            error!("Failed to save deletion for {}: {:?}", wd.uuid, e);
                                            continue;
                                        }
                                        local_state.path_map.remove_by_path(&wd.rel_path);
                                        if let Err(e) = local_state.path_map.save() {
                                            error!("Failed to save path_map: {:?}", e);
                                        }
                                        let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                                        if let Err(e) = ws_tx.send(Message::new(MsgType::Update, wd.uuid.clone(), update)).await {
                                            error!("Failed to send deletion update: {:?}", e);
                                        }
                                    }
                                }
                            }

                            // Process creates/modifies (unmatched creates + modifications)
                            for (i, wc) in batch_creates.iter().enumerate() {
                                if used_create_indices.contains(&i) { continue; } // was matched as rename

                                let is_newly_discovered = local_state.path_map.get_uuid(&wc.rel_path).is_none();
                                let uuid = local_state.get_or_create_uuid(&wc.rel_path);
                                let state_path = local_state.get_state_path_for_uuid(&uuid);

                                if let Some(parent) = state_path.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }

                                let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());
                                let text_ref = doc.get_or_insert_text("content");
                                let current_meta = read_meta_path(&doc);
                                // Take previous_sv BEFORE write_meta_path so the broadcast delta
                                // includes meta.path (needed by other clients to locate the file).
                                let previous_sv = doc.transact().state_vector();
                                if current_meta.as_deref() != Some(wc.rel_path.as_str()) {
                                    write_meta_path(&doc, &wc.rel_path);
                                }

                                let newly_subscribed = !subscribed_docs.contains(&uuid);
                                if newly_subscribed {
                                    let sv = doc.transact().state_vector().encode_v1();
                                    if let Err(e) = ws_tx.send(Message::new(MsgType::SyncStep1, uuid.clone(), sv)).await {
                                        error!("Failed to subscribe to new doc {}: {:?}", uuid, e);
                                    } else {
                                        subscribed_docs.insert(uuid.clone());
                                        if is_newly_discovered {
                                            freshly_created.insert(uuid.clone());
                                        }
                                    }
                                }

                                let yjs_content = text_ref.get_string(&doc.transact());
                                let needs_update = newly_subscribed
                                    || wc.disk_content != yjs_content
                                    || current_meta.as_deref() != Some(wc.rel_path.as_str());

                                if needs_update {
                                    info!("Local file {} changed, broadcasting...", wc.path.display());
                                    // previous_sv already captured before write_meta_path above
                                    apply_diff_to_yrs(&doc, &text_ref, &yjs_content, &wc.disk_content);
                                    if let Err(e) = save_doc(&doc, &state_path) {
                                        error!("Failed to save doc locally: {:?}", e);
                                        continue;
                                    }
                                    if let Err(e) = local_state.path_map.save() {
                                        error!("Failed to save path_map: {:?}", e);
                                    }
                                    let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                                    if let Err(e) = ws_tx.send(Message::new(MsgType::Update, uuid, update)).await {
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
/// For `rel_path = "notes/idea.md"` and `client_name = "laptop-abc123"`:
/// - attempt 0 → `"notes/idea (laptop-abc123).md"`
/// - attempt 1 → `"notes/idea (laptop-abc123 2).md"`
fn make_conflict_path(rel_path: &str, client_name: &str, attempt: u32) -> String {
    let path = std::path::Path::new(rel_path);
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
        .unwrap_or(rel_path);
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

/// Resolves a path collision: the server's UUID (server_uuid) wants to occupy
/// `canonical_path`, but a locally-freshly-created UUID (local_uuid) already
/// lives there.
///
/// Resolution: the server's version is canonical. The local doc is moved to a
/// conflict filename. Both are synced.
async fn resolve_path_conflict(
    state: &mut LocalState,
    server_uuid: &str,
    canonical_path: &str,
    local_uuid: &str,
    server_doc: &Doc,
    ws_tx: &mpsc::Sender<Message>,
    subscribed_docs: &mut HashSet<String>,
) -> anyhow::Result<()> {
    // Find a conflict filename that doesn't already exist on disk
    let conflict_rel_path = (0u32..)
        .map(|attempt| make_conflict_path(canonical_path, &state.client_name, attempt))
        .find(|candidate| !state.root_dir.join(candidate).exists())
        .expect("conflict filename search is unbounded");

    let conflict_phys = state.root_dir.join(&conflict_rel_path);
    let conflict_state_path = state.get_state_path_for_uuid(local_uuid);

    if let Some(parent) = conflict_phys.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = conflict_state_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Load local doc, update its meta.path to the conflict name
    let local_doc = load_doc(&conflict_state_path).unwrap_or_else(|_| Doc::new());
    let local_text_ref = local_doc.get_or_insert_text("content");
    let local_text = local_text_ref.get_string(&local_doc.transact());
    let local_previous_sv = local_doc.transact().state_vector();
    write_meta_path(&local_doc, &conflict_rel_path);
    save_doc(&local_doc, &conflict_state_path)?;

    // Write local content to the conflict file on disk
    fs::write(&conflict_phys, &local_text)?;

    // Update path_map: local UUID now at conflict path
    state.path_map.remove_by_path(canonical_path);
    state.path_map.insert(conflict_rel_path.clone(), local_uuid.to_string());

    // Write server content to canonical path
    let server_text_ref = server_doc.get_or_insert_text("content");
    let server_text = server_text_ref.get_string(&server_doc.transact());
    let canonical_phys = state.root_dir.join(canonical_path);
    if let Some(parent) = canonical_phys.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&canonical_phys, &server_text)?;

    // Save server doc to .bin under server_uuid
    let server_state_path = state.get_state_path_for_uuid(server_uuid);
    if let Some(parent) = server_state_path.parent() {
        fs::create_dir_all(parent)?;
    }
    save_doc(server_doc, &server_state_path)?;
    state.path_map.insert(canonical_path.to_string(), server_uuid.to_string());
    if let Err(e) = state.path_map.save() {
        warn!("Failed to save path_map after conflict resolution: {:?}", e);
    }

    info!(
        "Conflict resolved: '{}' keeps server content (UUID {}); local content saved as '{}' (UUID {})",
        canonical_path, server_uuid, conflict_rel_path, local_uuid
    );

    // Broadcast the conflict file (local UUID with new meta.path)
    let conflict_update = local_doc.transact().encode_state_as_update_v1(&local_previous_sv);
    if !subscribed_docs.contains(local_uuid) {
        let sv = StateVector::default().encode_v1();
        if let Err(e) = ws_tx.send(Message::new(MsgType::SyncStep1, local_uuid.to_string(), sv)).await {
            warn!("Failed to subscribe to conflict file {}: {:?}", local_uuid, e);
        } else {
            subscribed_docs.insert(local_uuid.to_string());
        }
    }
    if let Err(e) = ws_tx.send(Message::new(MsgType::Update, local_uuid.to_string(), conflict_update)).await {
        warn!("Failed to upload conflict file {}: {:?}", local_uuid, e);
    }

    Ok(())
}
