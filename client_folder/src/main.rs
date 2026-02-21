use client_folder::diff::apply_diff_to_yrs;
use client_folder::network::SynclineClient;
use client_folder::protocol::{Message, MsgType};
use client_folder::state::LocalState;
use client_folder::storage::{load_doc, save_doc};
use client_folder::watcher::DebouncedWatcher;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{Level, error, info};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .init();
    info!("Starting Syncline Client (Phase 4 & 5)");

    let args: Vec<String> = env::args().collect();
    let dir_to_watch = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        PathBuf::from(".")
    };

    let local_state = LocalState::new(&dir_to_watch);

    // Phase 5 part: Loop protection via expected hashes or checksums.
    // (We removed the simple hashset approach)

    // Setup network
    let server_url = "ws://127.0.0.1:3000/sync"; // Hardcoded default based on common dev usage
    let mut client = SynclineClient::new(server_url)?;

    let (app_tx, mut app_rx) = mpsc::channel(100);
    let ws_tx = client.connect(app_tx).await?;

    // Track which doc_ids we have subscribed to via SyncStep1 so we can
    // subscribe on-the-fly when the watcher discovers a new file.
    let mut subscribed_docs: HashSet<String> = HashSet::new();

    // Phase 4: send MSG_SYNC_STEP_1 for __index__ and all known documents
    if let Ok(docs) = local_state.list_doc_ids() {
        for doc_id in docs {
            let state_path = local_state.get_state_path(&doc_id);
            if let Ok(doc) = load_doc(&state_path) {
                let sv = doc.transact().state_vector().encode_v1();
                ws_tx
                    .send(Message::new(MsgType::SyncStep1, doc_id.clone(), sv))
                    .await?;
                subscribed_docs.insert(doc_id);
            }
        }
    }

    // Request sync for __index__
    ws_tx
        .send(Message::new(
            MsgType::SyncStep1,
            "__index__".to_string(),
            StateVector::default().encode_v1(),
        ))
        .await?;
    subscribed_docs.insert("__index__".to_string());

    // Phase 1 + 5: Setup debounced watcher
    let (watcher_tx, mut watcher_rx) = mpsc::channel(100);
    let mut watcher = DebouncedWatcher::new(watcher_tx, std::time::Duration::from_millis(300))?;
    watcher.watch(&dir_to_watch)?;

    info!("Listening for file events. Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            Some(msg) = app_rx.recv() => {
                match msg.msg_type {
                    MsgType::SyncStep2 | MsgType::Update => {
                        let doc_id = msg.doc_id;
                        if doc_id == "__index__" {
                            continue;
                        }

                        let state_path = local_state.get_state_path(&doc_id);
                        let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());

                        let phys_path = local_state.root_dir.join(&doc_id);

                        // 1. Check for unsynced local disk changes before we overwrite the disk
                        let text_ref = doc.get_or_insert_text("content");
                        let yjs_content_before = text_ref.get_string(&doc.transact());
                        let has_file = fs::metadata(&phys_path).is_ok();
                        let disk_content = fs::read_to_string(&phys_path).unwrap_or_default();

                        if has_file && yjs_content_before != disk_content {
                            info!("Unsynced local changes in {}! Diffing before applying remote update.", doc_id);
                            apply_diff_to_yrs(&doc, &text_ref, &yjs_content_before, &disk_content);

                            // Broadcast the local edits so the other remote peers get them too
                            let state_vector = StateVector::default(); // Note: could be optimized later instead of full state transmit
                            let local_update = doc.transact().encode_state_as_update_v1(&state_vector);
                            if let Err(e) = ws_tx.send(Message::new(MsgType::Update, doc_id.clone(), local_update)).await {
                                error!("Failed to send unsynced local update: {:?}", e);
                            }
                        }

                        // 2. NOW apply the remote update
                        if let Ok(update) = Update::decode_v1(&msg.payload) {
                            let mut txn = doc.transact_mut();
                            if let Err(e) = txn.apply_update(update) {
                                error!("Failed to apply update for {}: {:?}", doc_id, e);
                                continue;
                            }

                            let text_val = text_ref.get_string(&txn);
                            drop(txn);

                            if let Err(e) = save_doc(&doc, &state_path) {
                                error!("Failed to save doc state {}: {:?}", doc_id, e);
                            }

                            // expected_writes removed


                            if let Some(parent) = phys_path.parent() {
                                let _ = fs::create_dir_all(parent);
                            }

                            if let Err(e) = fs::write(&phys_path, text_val) {
                                error!("Failed to write physical file {}: {:?}", phys_path.display(), e);
                            } else {
                                info!("Applied remote update to file {}", phys_path.display());
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

                            // try matching on canonicalized path just in case
                            /*
                            if let Ok(canon) = std::fs::canonicalize(&path) {
                                // we removed expected_writes entirely
                            }
                            */

                            if !path.is_file() { continue; }
                            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                            if ext != "md" && ext != "txt" { continue; }
                            if path.to_string_lossy().contains(".syncline") || path.to_string_lossy().contains(".git") { continue; }

                            let doc_id = match local_state.get_doc_id(&path) {
                                Ok(d) => d,
                                Err(_) => continue,
                            };
                            let state_path = local_state.get_state_path(&doc_id);

                            let doc = load_doc(&state_path).unwrap_or_else(|_| Doc::new());
                            let text_ref = doc.get_or_insert_text("content");

                            // If this document has never been subscribed to, send SyncStep1
                            // first so the server creates a broadcast channel for it and we
                            // receive updates from other clients.
                            if !subscribed_docs.contains(&doc_id) {
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

                            let disk_content = match fs::read_to_string(&path) {
                                Ok(c) => c,
                                Err(_) => continue,
                            };

                            let yjs_content = text_ref.get_string(&doc.transact());
                            if disk_content != yjs_content {
                                info!("Local file {} modified, diffing and broadcasting...", path.display());
                                apply_diff_to_yrs(&doc, &text_ref, &yjs_content, &disk_content);
                                if let Err(e) = save_doc(&doc, &state_path) {
                                    error!("Failed to save doc locally: {:?}", e);
                                    continue;
                                }

                                let update = doc.transact().encode_state_as_update_v1(&StateVector::default());
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
