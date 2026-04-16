use crate::client::diff::apply_diff_to_yrs;
use crate::client::network::SynclineClient;
use crate::client::protocol::{Message, MsgType};
use crate::client::state::{
    is_binary_file, read_meta_blob_hash, read_meta_path, read_meta_type, sha256_hash,
    write_meta_blob_hash, write_meta_path, write_meta_type, BlobChange, LocalState,
};
use crate::client::storage::{load_doc, save_doc};
use crate::client::watcher::DebouncedWatcher;
use colored::Colorize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
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
    let (initial_offline_changes, mut freshly_created, initial_blob_changes) =
        match local_state.bootstrap_offline_changes() {
            Ok(result) => result,
            Err(e) => {
                error!("Error bootstrapping offline changes: {:?}", e);
                (Vec::new(), HashSet::new(), Vec::new())
            }
        };
    // Use take() so offline changes are broadcast exactly once (on first
    // successful connection). On subsequent reconnects this is None.
    let mut pending_offline_changes: Option<Vec<(String, Vec<u8>)>> =
        Some(initial_offline_changes);
    let mut pending_blob_changes: Option<Vec<BlobChange>> = Some(initial_blob_changes);

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
        // Snapshot of the server's __index__ at first connection. Used by
        // collision detection to distinguish "server had this before I
        // connected" from "both clients uploaded simultaneously."
        let mut initial_server_uuids: Option<HashSet<String>> = None;

        // Phase 4: send MSG_SYNC_STEP_1 for all known content documents.
        // Skip __index__ here — it is subscribed separately below with a
        // fresh state vector.  Sending it from list_doc_ids first would race:
        // the server may reply with an empty __index__ (content UUIDs not yet
        // registered), causing the client to falsely delete local files.
        if let Ok(uuids) = local_state.list_doc_ids() {
            for uuid in uuids {
                if uuid == "__index__" || uuid.starts_with("data/") && uuid.contains("__index__") {
                    continue;
                }
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

        // Upload pending blobs (first connection only)
        let blob_changes = pending_blob_changes.take().unwrap_or_default();
        for blob in blob_changes {
            if let Err(e) = ws_tx
                .send(Message::new(MsgType::BlobUpdate, blob.uuid.clone(), blob.data))
                .await
            {
                error!("Failed to upload blob for {}: {:?}", blob.uuid, e);
            } else {
                info!("Uploaded blob for {} (hash: {})", blob.uuid, blob.hash);
            }
        }

        // Periodic reconciliation: re-send state vectors to catch any updates
        // lost due to broadcast receiver lag or transient network issues.
        let mut resync_interval = tokio::time::interval(std::time::Duration::from_secs(60));
        resync_interval.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                _ = resync_interval.tick() => {
                    let doc_ids: Vec<String> = subscribed_docs.iter().cloned().collect();
                    for doc_id in doc_ids {
                        let state_path = local_state.get_state_path_for_uuid(&doc_id);
                        if let Ok(doc) = load_doc(&state_path) {
                            let sv = doc.transact().state_vector().encode_v1();
                            if let Err(e) = ws_tx.send(Message::new(MsgType::Resync, doc_id.clone(), sv)).await {
                                error!("Failed to send resync for {}: {:?}", doc_id, e);
                                break;
                            }
                        }
                    }
                    debug!("Periodic resync sent for {} docs", subscribed_docs.len());
                }

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
                                        // This snapshot lets collision detection know which docs
                                        // were already on the server before we connected.
                                        if initial_server_uuids.is_none() {
                                            initial_server_uuids = Some(
                                                new_index_uuids.iter().map(|s| s.to_string()).collect()
                                            );
                                        }

                                        // Detect UUID removals from index → delete local files.
                                        // Never delete freshly-created UUIDs — they are local
                                        // files that haven't been synced to the server yet and
                                        // therefore won't appear in the server's __index__.
                                        let mut to_remove = Vec::new();
                                        for sub_uuid in &subscribed_docs {
                                            if sub_uuid == "__index__" { continue; }
                                            if freshly_created.contains(sub_uuid.as_str()) { continue; }
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
                                if let Ok(disk_content) = fs::read_to_string(&phys_path)
                                    && disk_content != yjs_content_before {
                                        info!("Unsynced local changes in {}! Applying before remote update.", rp);
                                        let prev_sv = doc.transact().state_vector();
                                        apply_diff_to_yrs(&doc, &text_ref, &yjs_content_before, &disk_content);
                                        let local_update = doc.transact().encode_state_as_update_v1(&prev_sv);
                                        if let Err(e) = ws_tx.send(Message::new(MsgType::Update, doc_id.clone(), local_update)).await {
                                            error!("Failed to send unsynced local update: {:?}", e);
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
                                let collision_local_uuid = detect_path_collision(
                                    &local_state.path_map,
                                    &doc_id,
                                    &target_rel_path,
                                    &freshly_created,
                                    &initial_server_uuids,
                                );

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
                            let meta_type = read_meta_type(&doc)
                                .unwrap_or_else(|| "text".to_string());

                            if meta_type == "binary" {
                                // Binary file: check blob_hash
                                let blob_hash = read_meta_blob_hash(&doc);
                                match blob_hash.as_deref() {
                                    Some("") | None => {
                                        // Empty or missing hash → deletion
                                        if phys_path.exists() {
                                            if let Err(e) = fs::remove_file(&phys_path) {
                                                error!("Failed to delete binary file {:?}: {:?}", phys_path, e);
                                            } else {
                                                info!("Deleted binary file {} (empty blob_hash)", phys_path.display());
                                            }
                                        }
                                    }
                                    Some(hash) => {
                                        // Check if local file already matches
                                        let local_hash = fs::read(&phys_path)
                                            .ok()
                                            .map(|d| sha256_hash(&d));
                                        if local_hash.as_deref() != Some(hash) {
                                            // Need to request blob from server
                                            info!("Binary file {} needs blob {} from server", target_rel_path, hash);
                                            let request_payload = hash.as_bytes().to_vec();
                                            if let Err(e) = ws_tx
                                                .send(Message::new(
                                                    MsgType::BlobRequest,
                                                    doc_id.clone(),
                                                    request_payload,
                                                ))
                                                .await
                                            {
                                                error!("Failed to request blob: {:?}", e);
                                            }
                                        }
                                    }
                                }
                            } else {
                                // Text file: write content
                                if text_val.is_empty() {
                                    if phys_path.exists() {
                                        if let Err(e) = fs::remove_file(&phys_path) {
                                            error!("Failed to delete file {:?}: {:?}", phys_path, e);
                                        } else {
                                            info!("Deleted file {} (empty content)", phys_path.display());
                                        }
                                    }
                                } else if let Some(parent) = phys_path.parent() {
                                    let _ = fs::create_dir_all(parent);
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
                        }
                        MsgType::BlobUpdate => {
                            // Received blob data from server (in response to BlobRequest)
                            let doc_id = msg.doc_id;
                            let blob_data = msg.payload;

                            // Find the physical path for this UUID.
                            // Primary: look up path_map (fast, in-memory).
                            // Fallback: read meta.path from the persisted .bin doc
                            // to handle the race where a BlobUpdate arrives before
                            // the SyncStep2/Update that populates path_map.
                            let rel_path = local_state.path_map.get_path_for_uuid(&doc_id).map(|s| s.to_string())
                                .or_else(|| {
                                    let state_path = local_state.get_state_path_for_uuid(&doc_id);
                                    load_doc(&state_path).ok().and_then(|doc| {
                                        let p = read_meta_path(&doc);
                                        if let Some(ref path) = p {
                                            // Populate path_map so future lookups succeed
                                            local_state.path_map.insert(path.clone(), doc_id.clone());
                                            let _ = local_state.path_map.save();
                                        }
                                        p
                                    })
                                });

                            if let Some(rel_path) = rel_path {
                                let phys_path = local_state.root_dir.join(&rel_path);
                                if let Some(parent) = phys_path.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }
                                if let Err(e) = fs::write(&phys_path, &blob_data) {
                                    error!("Failed to write binary file {:?}: {:?}", phys_path, e);
                                } else {
                                    info!("Downloaded binary file {} ({} bytes)", phys_path.display(), blob_data.len());
                                }
                            } else {
                                warn!("Received blob for unknown UUID {}", doc_id);
                            }
                        }
                        MsgType::SyncStep1 | MsgType::BlobRequest | MsgType::Resync => {}
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
                            struct WatchBinaryCreate {
                                path: std::path::PathBuf,
                                rel_path: String,
                                data: Vec<u8>,
                                hash: String,
                            }

                            let mut batch_deletes: Vec<WatchDelete> = Vec::new();
                            let mut batch_creates: Vec<WatchCreate> = Vec::new();
                            let mut batch_binary_creates: Vec<WatchBinaryCreate> = Vec::new();

                            for ev in &events {
                                let path = &ev.path;

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
                                    if is_binary_file(std::path::Path::new(&rel_path)) {
                                        // Binary file
                                        if let Ok(data) = fs::read(path) {
                                            let hash = sha256_hash(&data);
                                            batch_binary_creates.push(WatchBinaryCreate {
                                                path: path.clone(),
                                                rel_path,
                                                data,
                                                hash,
                                            });
                                        }
                                    } else if let Ok(content) = fs::read_to_string(path) {
                                        batch_creates.push(WatchCreate {
                                            path: path.clone(),
                                            rel_path,
                                            disk_content: content,
                                        });
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

                            // Process binary file creates/modifies
                            for bwc in &batch_binary_creates {
                                let is_newly_discovered = local_state.path_map.get_uuid(&bwc.rel_path).is_none();
                                let uuid = local_state.get_or_create_uuid(&bwc.rel_path);
                                let state_path = local_state.get_state_path_for_uuid(&uuid);

                                if let Some(parent) = state_path.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }

                                let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());
                                let current_meta = read_meta_path(&doc);
                                let current_hash = read_meta_blob_hash(&doc);
                                let previous_sv = doc.transact().state_vector();

                                // Update metadata
                                let meta_changed = current_meta.as_deref() != Some(bwc.rel_path.as_str());
                                let hash_changed = current_hash.as_deref() != Some(bwc.hash.as_str());

                                if meta_changed {
                                    write_meta_path(&doc, &bwc.rel_path);
                                }
                                write_meta_type(&doc, "binary");
                                if hash_changed {
                                    write_meta_blob_hash(&doc, &bwc.hash);
                                }

                                let newly_subscribed = !subscribed_docs.contains(&uuid);
                                if newly_subscribed {
                                    let sv = doc.transact().state_vector().encode_v1();
                                    if let Err(e) = ws_tx.send(Message::new(MsgType::SyncStep1, uuid.clone(), sv)).await {
                                        error!("Failed to subscribe to new binary doc {}: {:?}", uuid, e);
                                    } else {
                                        subscribed_docs.insert(uuid.clone());
                                        if is_newly_discovered {
                                            freshly_created.insert(uuid.clone());
                                        }
                                    }
                                }

                                let needs_update = newly_subscribed || meta_changed || hash_changed;
                                if needs_update {
                                    info!("Binary file {} changed, broadcasting...", bwc.path.display());
                                    if let Err(e) = save_doc(&doc, &state_path) {
                                        error!("Failed to save binary doc locally: {:?}", e);
                                        continue;
                                    }
                                    if let Err(e) = local_state.path_map.save() {
                                        error!("Failed to save path_map: {:?}", e);
                                    }
                                    // Send CRDT update (meta changes)
                                    let update = doc.transact().encode_state_as_update_v1(&previous_sv);
                                    if let Err(e) = ws_tx.send(Message::new(MsgType::Update, uuid.clone(), update)).await {
                                        error!("Failed to send binary meta update: {:?}", e);
                                    }
                                    // Send blob data
                                    if hash_changed
                                        && let Err(e) = ws_tx.send(Message::new(MsgType::BlobUpdate, uuid, bwc.data.clone())).await {
                                            error!("Failed to upload blob: {:?}", e);
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

/// Check if an incoming document (identified by `incoming_uuid` wanting
/// `incoming_path`) collides with a locally-created document.
///
/// Returns `Some(local_uuid)` if a freshly-created local doc occupies
/// `incoming_path` and should be conflict-resolved.
///
/// Two-tier collision guard:
///
/// 1. **Asymmetric case** (one client joined after the other uploaded):
///    If `incoming_uuid` is in `initial_server_uuids`, the server had it
///    before we connected → always resolve (our doc is the latecomer).
///
/// 2. **Symmetric case** (both connected simultaneously, `initial_server_uuids`
///    is empty): Use a deterministic tie-breaker — only the client whose
///    local UUID is lexicographically GREATER moves its doc to a conflict
///    path. This ensures exactly one client resolves.
///
/// Race condition found by TLA+ formal verification (Phase 6): without
/// the tie-breaker, two clients connecting before either uploads would both
/// have empty `initial_server_uuids`, so neither would detect the collision.
pub(crate) fn detect_path_collision(
    path_map: &crate::client::state::PathMap,
    incoming_uuid: &str,
    incoming_path: &str,
    freshly_created: &HashSet<String>,
    initial_server_uuids: &Option<HashSet<String>>,
) -> Option<String> {
    path_map
        .get_uuid(incoming_path)
        .and_then(|local_uuid| {
            if local_uuid != incoming_uuid
                && freshly_created.contains(local_uuid)
            {
                let initial = initial_server_uuids.as_ref();
                // Tier 1: incoming was on server at connect time → always resolve
                let in_initial = initial.map_or(false, |set| set.contains(incoming_uuid));
                // Tier 2: both connected simultaneously (empty initial) → tie-breaker
                let initial_empty = initial.map_or(true, |set| set.is_empty());
                let tiebreak = initial_empty && local_uuid > incoming_uuid;

                if in_initial || tiebreak {
                    Some(local_uuid.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Symmetric race (Phase 6 TLA+ bug): both clients connect simultaneously,
    /// initial_server_uuids is empty. Tie-breaker: higher UUID yields.
    #[test]
    fn test_path_collision_race_both_connect_before_upload() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let mut path_map = state.path_map;
        path_map.insert("file.md".to_string(), "uuid-b".to_string());

        let mut freshly_created = HashSet::new();
        freshly_created.insert("uuid-b".to_string());

        // Both connected simultaneously → initial_server_uuids is empty
        let initial: Option<HashSet<String>> = Some(HashSet::new());

        let collision = detect_path_collision(
            &path_map,
            "uuid-a",
            "file.md",
            &freshly_created,
            &initial,
        );

        assert!(collision.is_some(), "Tie-breaker: uuid-b > uuid-a → this client resolves");
        assert_eq!(collision.unwrap(), "uuid-b");
    }

    /// Asymmetric: incoming UUID in initial_server_uuids → always resolve.
    #[test]
    fn test_path_collision_detected_when_uuid_in_initial_snapshot() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let mut path_map = state.path_map;
        path_map.insert("file.md".to_string(), "uuid-b".to_string());

        let mut freshly_created = HashSet::new();
        freshly_created.insert("uuid-b".to_string());

        let mut initial_set = HashSet::new();
        initial_set.insert("uuid-a".to_string());
        let initial: Option<HashSet<String>> = Some(initial_set);

        let collision = detect_path_collision(
            &path_map,
            "uuid-a",
            "file.md",
            &freshly_created,
            &initial,
        );

        assert!(collision.is_some(), "Asymmetric: incoming in initial → always resolve");
        assert_eq!(collision.unwrap(), "uuid-b");
    }

    /// Asymmetric: fires regardless of UUID ordering when incoming is in initial.
    #[test]
    fn test_path_collision_asymmetric_regardless_of_uuid_order() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let mut path_map = state.path_map;
        path_map.insert("file.md".to_string(), "uuid-a".to_string());

        let mut freshly_created = HashSet::new();
        freshly_created.insert("uuid-a".to_string());

        let mut initial_set = HashSet::new();
        initial_set.insert("uuid-b".to_string());
        let initial: Option<HashSet<String>> = Some(initial_set);

        let collision = detect_path_collision(
            &path_map,
            "uuid-b",
            "file.md",
            &freshly_created,
            &initial,
        );

        assert!(collision.is_some(), "Asymmetric guard fires even when local < incoming");
        assert_eq!(collision.unwrap(), "uuid-a");
    }

    /// No collision when the incoming UUID matches the local UUID at that path.
    #[test]
    fn test_no_collision_same_uuid() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let mut path_map = state.path_map;
        path_map.insert("file.md".to_string(), "uuid-a".to_string());

        let mut freshly_created = HashSet::new();
        freshly_created.insert("uuid-a".to_string());

        let collision = detect_path_collision(
            &path_map,
            "uuid-a",
            "file.md",
            &freshly_created,
            &None,
        );

        assert!(collision.is_none(), "No collision when UUIDs match");
    }

    /// Symmetric tie-breaker: local < incoming with empty initial → no collision.
    #[test]
    fn test_no_collision_when_local_uuid_is_lower_symmetric() {
        let dir = tempdir().unwrap();
        let state = LocalState::new(dir.path(), Some("test-client".to_string()));

        let mut path_map = state.path_map;
        path_map.insert("file.md".to_string(), "uuid-a".to_string());

        let mut freshly_created = HashSet::new();
        freshly_created.insert("uuid-a".to_string());

        let initial: Option<HashSet<String>> = Some(HashSet::new());

        let collision = detect_path_collision(
            &path_map,
            "uuid-b",
            "file.md",
            &freshly_created,
            &initial,
        );

        assert!(collision.is_none(), "Tie-breaker: uuid-a < uuid-b → other client resolves");
    }
}
