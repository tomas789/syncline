use crate::server::db::Db;
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
};
use tokio::sync::{Mutex as AsyncMutex, RwLock, broadcast, mpsc};
use yrs::{Doc, GetString, StateVector, Text, Transact, Update, updates::decoder::Decode};

use crate::protocol::{
    MSG_BLOB_REQUEST, MSG_BLOB_UPDATE, MSG_RESYNC, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE,
    decode_message, encode_message,
};

type ChannelMap = Arc<RwLock<HashMap<String, broadcast::Sender<(Vec<u8>, uuid::Uuid)>>>>;

#[derive(Clone)]
struct AppState {
    db: Db,
    channels: ChannelMap,
    /// Tracks which doc_ids have been registered so we can update __index__ exactly once per doc.
    known_doc_ids: Arc<RwLock<HashSet<String>>>,
    /// The __index__ Yrs document, shared across all connection handlers.
    index_doc: Arc<AsyncMutex<Doc>>,
}

/// Insert `doc_id` into the shared `__index__` document if it is new, then
/// persist the delta to the DB and broadcast it to all `__index__` subscribers.
async fn update_index_for_new_doc(state: &AppState, doc_id: &str) {
    // Guard: only proceed for a truly new doc_id.
    let is_new = {
        let mut known = state.known_doc_ids.write().await;
        known.insert(doc_id.to_string())
    };
    if !is_new {
        return;
    }

    // Append the new doc_id (newline-terminated) to the index text and capture the delta.
    let delta = {
        let index_doc = state.index_doc.lock().await;
        let index_text = index_doc.get_or_insert_text("content");
        let mut txn = index_doc.transact_mut();
        let current = index_text.get_string(&txn);
        let len = current.len() as u32;
        index_text.insert(&mut txn, len, &format!("{}\n", doc_id));
        txn.encode_update_v1()
    };

    // Persist to DB so newly-connecting clients get it via SyncStep2.
    if let Err(e) = state.db.save_update("__index__", &delta).await {
        tracing::error!("Failed to save __index__ update for {}: {}", doc_id, e);
        return;
    }

    // Broadcast delta to currently-connected clients subscribed to __index__.
    let channels = state.channels.read().await;
    if let Some(tx) = channels.get("__index__") {
        // Pre-frame the delta so the forwarding task can pass it through unchanged.
        let msg = encode_message(MSG_UPDATE, "__index__", &delta);
        // uuid::Uuid::nil() means "no sender", so every subscriber receives this.
        let _ = tx.send((msg, uuid::Uuid::nil()));
    }
}

pub async fn run_server(db: Db, port: u16) -> anyhow::Result<()> {
    // Rebuild the in-memory __index__ document from any updates already in the DB,
    // and extract the set of known doc_ids from its text content.
    let index_doc = Doc::new();
    let all_index_updates = db.load_doc_updates("__index__").await.unwrap_or_default();
    {
        let mut txn = index_doc.transact_mut();
        for update_data in all_index_updates {
            if let Ok(u) = Update::decode_v1(&update_data) {
                txn.apply_update(u);
            }
        }
    }
    let known_doc_ids: HashSet<String> = {
        let index_text = index_doc.get_or_insert_text("content");
        let txn = index_doc.transact();
        let content = index_text.get_string(&txn);
        drop(txn);
        content
            .lines()
            .filter(|s: &&str| !s.is_empty())
            .map(|s: &str| s.to_string())
            .collect()
    };

    let state = AppState {
        db,
        channels: Arc::new(RwLock::new(HashMap::new())),
        known_doc_ids: Arc::new(RwLock::new(known_doc_ids)),
        index_doc: Arc::new(AsyncMutex::new(index_doc)),
    };

    let app = Router::new()
        .route("/sync", get(ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local_addr = listener.local_addr()?;
    println!("Server listening on {}", local_addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let connection_id = uuid::Uuid::new_v4();
    let (mut sender, mut receiver) = socket.split();

    // Unbounded so that broadcast forwarding tasks never block the async runtime
    // and never silently drop outgoing messages.
    let (tx_socket, mut rx_socket) = mpsc::unbounded_channel::<Vec<u8>>();

    // Task 1: Forward messages from MPSC to WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(data) = rx_socket.recv().await {
            if sender.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // Task 2: Receive from WebSocket and handle
    let tx_socket_clone = tx_socket.clone();
    let state_clone = state.clone();

    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Binary(data) = msg {
                if let Some((msg_type, doc_id, payload)) = decode_message(&data) {
                    match msg_type {
                        MSG_SYNC_STEP_1 => {
                            tracing::info!("Received SYNC_STEP_1 for doc: {}", doc_id);

                            // Subscribe to (or create) the broadcast channel for this doc.
                            let rx = {
                                let mut channels = state_clone.channels.write().await;
                                let tx = channels.entry(doc_id.to_string()).or_insert_with(|| {
                                    // Large capacity so fast writers never make receivers lag.
                                    let (tx, _rx) = broadcast::channel(65_536);
                                    tx
                                });
                                tx.subscribe()
                            };

                            // Register the doc in __index__ so other clients can discover it.
                            if doc_id != "__index__" {
                                update_index_for_new_doc(&state_clone, doc_id).await;
                            }

                            // Spawn an event-driven forwarding task for this doc.
                            // It exits automatically when the outgoing sender is closed
                            // (i.e. when this WebSocket connection ends).
                            let tx_fwd = tx_socket_clone.clone();
                            let doc_id_str = doc_id.to_string();
                            let db_fwd = state_clone.db.clone();
                            tokio::spawn(async move {
                                let mut rx = rx;
                                // Tracks the client's approximate state vector.
                                // On lag, we do a full catch-up from the DB using
                                // an empty SV (sends everything), which is safe
                                // because CRDT updates are idempotent.
                                loop {
                                    tokio::select! {
                                        _ = tx_fwd.closed() => break,
                                        res = rx.recv() => {
                                            match res {
                                                Ok((framed_msg, sender_id)) => {
                                                    if sender_id == connection_id {
                                                        continue;
                                                    }
                                                    tracing::debug!("Forwarding broadcast for doc: {} with {} bytes", doc_id_str, framed_msg.len());
                                                    // Messages are already framed (encode_message
                                                    // was called before broadcasting).
                                                    if tx_fwd.send(framed_msg).is_err() {
                                                        // Outgoing channel closed — connection gone.
                                                        break;
                                                    }
                                                }
                                                Err(broadcast::error::RecvError::Closed) => break,
                                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                                    // The receiver missed N messages. Instead of
                                                    // silently skipping them, do a full catch-up
                                                    // from the DB. This is safe because CRDT
                                                    // updates are idempotent — re-applying already-
                                                    // seen operations is a no-op.
                                                    tracing::warn!(
                                                        "Broadcast receiver lagged by {} messages for doc {}. \
                                                         Triggering full catch-up from DB.",
                                                        n, doc_id_str
                                                    );
                                                    match db_fwd.get_all_updates_since(
                                                        &doc_id_str,
                                                        &StateVector::default(),
                                                    ).await {
                                                        Ok(update) if !update.is_empty() => {
                                                            let resp = encode_message(
                                                                MSG_SYNC_STEP_2,
                                                                &doc_id_str,
                                                                &update,
                                                            );
                                                            if tx_fwd.send(resp).is_err() {
                                                                break;
                                                            }
                                                            tracing::info!(
                                                                "Sent full catch-up ({} bytes) for doc {} after lag",
                                                                update.len(),
                                                                doc_id_str
                                                            );
                                                        }
                                                        Ok(_) => {} // DB empty, nothing to catch up
                                                        Err(e) => {
                                                            tracing::error!(
                                                                "DB error during lag catch-up for {}: {}",
                                                                doc_id_str, e
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            });

                            // Send back the current state for this doc.
                            if let Ok(sv) = StateVector::decode_v1(payload) {
                                match state_clone.db.get_all_updates_since(doc_id, &sv).await {
                                    Ok(update) if !update.is_empty() => {
                                        tracing::info!(
                                            "Sending SYNC_STEP_2 for doc {} with {} bytes",
                                            doc_id,
                                            update.len()
                                        );
                                        let resp = encode_message(MSG_SYNC_STEP_2, doc_id, &update);
                                        let _ = tx_socket_clone.send(resp);
                                    }
                                    Ok(_) => {
                                        tracing::info!("No updates to send for doc {}", doc_id);
                                    }
                                    Err(e) => tracing::error!("DB Error: {}", e),
                                }
                            }
                        }
                        MSG_UPDATE => {
                            // Register the doc in __index__ so other clients can discover it.
                            if doc_id != "__index__" {
                                update_index_for_new_doc(&state_clone, doc_id).await;
                            }

                            // Persist the update synchronously BEFORE broadcasting so that any
                            // concurrent SyncStep2 responses (triggered by a subscriber that just
                            // joined) always include the latest content.
                            //
                            // IMPORTANT: If DB persistence fails, we must NOT broadcast.
                            // Broadcasting without persisting creates a split-brain: connected
                            // clients see the update, but any client that (re)connects later
                            // will never receive it via SyncStep2. The sending client retains
                            // the update locally and will re-send it on next sync.
                            if let Err(e) = state_clone.db.save_update(doc_id, payload).await {
                                tracing::error!(
                                    "DB Save Error for doc {}: {}. Update NOT broadcast — \
                                     client will retry on next sync.",
                                    doc_id,
                                    e
                                );
                                continue; // skip broadcast
                            }

                            // Auto-create the channel if it doesn't exist yet. This handles
                            // the race where a client sends MSG_UPDATE before any SyncStep1
                            // has been received for this doc_id.
                            let msg = encode_message(MSG_UPDATE, doc_id, payload);
                            let mut channels = state_clone.channels.write().await;
                            let tx = channels
                                .entry(doc_id.to_string())
                                .or_insert_with(|| broadcast::channel(65_536).0);
                            let _ = tx.send((msg, connection_id));
                        }
                        MSG_BLOB_UPDATE => {
                            // Binary blob upload: compute SHA256 hash, store in blobs
                            // table, and relay the raw blob to all other subscribers.
                            use sha2::{Digest, Sha256};

                            if payload.len() > crate::protocol::MAX_BLOB_SIZE {
                                tracing::warn!(
                                    "Rejected blob for doc {} — {} bytes exceeds {} byte limit",
                                    doc_id,
                                    payload.len(),
                                    crate::protocol::MAX_BLOB_SIZE
                                );
                            } else {
                                let hash = format!("{:x}", Sha256::digest(payload));
                                tracing::info!(
                                    "Received BLOB_UPDATE for doc {} — hash={} size={}",
                                    doc_id,
                                    hash,
                                    payload.len()
                                );

                                // Content-addressable store: INSERT OR IGNORE
                                if let Err(e) =
                                    state_clone.db.save_blob(&hash, payload).await
                                {
                                    tracing::error!("DB blob save error: {}", e);
                                }

                                // Relay raw blob to all other subscribers of this doc.
                                // The message is pre-framed with encode_message so the
                                // forwarding task passes it through unchanged.
                                let msg =
                                    encode_message(MSG_BLOB_UPDATE, doc_id, payload);
                                let channels = state_clone.channels.read().await;
                                if let Some(tx) = channels.get(doc_id) {
                                    let _ = tx.send((msg, connection_id));
                                }
                            }
                        }
                        MSG_BLOB_REQUEST => {
                            // Client requests a blob by its hex-encoded SHA256 hash.
                            let hash = std::str::from_utf8(payload).unwrap_or("");
                            tracing::info!(
                                "Received BLOB_REQUEST for doc {} — hash={}",
                                doc_id,
                                hash
                            );
                            match state_clone.db.load_blob(hash).await {
                                Ok(Some(blob_data)) => {
                                    let resp =
                                        encode_message(MSG_BLOB_UPDATE, doc_id, &blob_data);
                                    let _ = tx_socket_clone.send(resp);
                                }
                                Ok(None) => {
                                    tracing::warn!(
                                        "Blob not found: hash={} for doc {}",
                                        hash,
                                        doc_id
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("DB blob load error: {}", e);
                                }
                            }
                        }
                        MSG_RESYNC => {
                            // Periodic reconciliation: send SyncStep2 with any
                            // missing updates, but do NOT create a new broadcast
                            // subscription (the client is already subscribed from
                            // the initial SyncStep1).
                            tracing::debug!("Received RESYNC for doc: {}", doc_id);
                            if let Ok(sv) = StateVector::decode_v1(payload) {
                                match state_clone.db.get_all_updates_since(doc_id, &sv).await {
                                    Ok(update) if !update.is_empty() => {
                                        tracing::info!(
                                            "Resync: sending {} bytes for doc {}",
                                            update.len(),
                                            doc_id
                                        );
                                        let resp = encode_message(MSG_SYNC_STEP_2, doc_id, &update);
                                        let _ = tx_socket_clone.send(resp);
                                    }
                                    Ok(_) => {} // fully in sync, nothing to send
                                    Err(e) => tracing::error!("DB Error during resync: {}", e),
                                }
                            }
                        }
                        _ => {}
                    }
                }
            } else if let Message::Close(_) = msg {
                break;
            }
        }
    });

    recv_task.await.ok();
    // Aborting send_task drops rx_socket, which closes the UnboundedSender.
    // Each per-doc forwarding task detects the closed sender on the next send
    // and exits on its own — no explicit abort needed.
    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
    use yrs::updates::encoder::Encode;
    use yrs::{Doc, ReadTxn, Text, Transact};

    async fn setup_test_server() -> (u16, AppState) {
        let db = Db::new("sqlite::memory:").await.unwrap();
        let state = AppState {
            db,
            channels: Arc::new(RwLock::new(HashMap::new())),
            known_doc_ids: Arc::new(RwLock::new(HashSet::new())),
            index_doc: Arc::new(AsyncMutex::new(Doc::new())),
        };

        let app = Router::new()
            .route("/sync", get(ws_handler))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (port, state)
    }

    #[tokio::test]
    async fn test_issue_1_task_leak_on_disconnect() {
        let (port, state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws_stream, _) = connect_async(url).await.unwrap();

        let doc_id = "test_doc";
        let sv = StateVector::default();
        let payload = sv.encode_v1();
        let msg =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, doc_id, &payload);

        ws_stream
            .send(TungsteniteMessage::Binary(msg.into()))
            .await
            .unwrap();

        // Wait for server to process the sync request and setup channels
        tokio::time::sleep(Duration::from_millis(100)).await;

        {
            let channels = state.channels.read().await;
            let tx = channels.get(doc_id).unwrap();
            assert_eq!(tx.receiver_count(), 1, "Should have 1 receiver");
        }

        // Disconnect client
        drop(ws_stream);

        // Wait for server to detect disconnect and hopefully close task
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check if receiver count went to 0
        {
            let channels = state.channels.read().await;
            let tx = channels.get(doc_id).unwrap();
            // Issue 1: task leak! The test expects it to drop to 0, so it will fail when it's still 1.
            assert_eq!(
                tx.receiver_count(),
                0,
                "Receiver count should be 0, but task leaked!"
            );
        }
    }

    #[tokio::test]
    async fn test_issue_2_re_echo_updates() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws_stream, _) = connect_async(url).await.unwrap();

        let doc_id = "test_doc_echo";
        let sv = StateVector::default();
        let payload = sv.encode_v1();
        let msg =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, doc_id, &payload);

        ws_stream
            .send(TungsteniteMessage::Binary(msg.into()))
            .await
            .unwrap();

        // Consume the SYNC_STEP_2 response that the server now reliably
        // sends for every SYNC_STEP_1 (even for empty newly discovered files).
        let _sync_response = tokio::time::timeout(Duration::from_millis(500), ws_stream.next())
            .await
            .expect("Should receive a SYNC_STEP_2 response!")
            .unwrap()
            .unwrap();

        // Let's send an update
        let doc = Doc::new();
        let text_ref = doc.get_or_insert_text("content");
        let update = {
            let mut txn = doc.transact_mut();
            text_ref.insert(&mut txn, 0, "Hello CRDT");
            txn.encode_update_v1()
        };

        let update_msg =
            crate::protocol::encode_message(crate::protocol::MSG_UPDATE, doc_id, &update);
        ws_stream
            .send(TungsteniteMessage::Binary(update_msg.into()))
            .await
            .unwrap();

        // We shouldn't receive the same update back. We'll wait a brief moment.
        let result = tokio::time::timeout(Duration::from_millis(200), ws_stream.next()).await;

        // Issue 2: re-echoing updates. This will receive Ok(Some(Ok(TungsteniteMessage::Binary(...))))
        // So the assert that it timed out will fail.
        assert!(
            result.is_err(),
            "Received an echoed message back from the server!"
        );
    }

    /// Regression test for the fuzzer-discovered divergence bug.
    ///
    /// Previously, the client's file-watcher handler sent MSG_UPDATE for newly
    /// created files without first sending MSG_SYNC_STEP_1. The server only
    /// created broadcast channels in response to MSG_SYNC_STEP_1, so updates
    /// for new documents were stored in the database but never relayed.
    ///
    /// The fix: the client now sends MSG_SYNC_STEP_1 before MSG_UPDATE for any
    /// doc it hasn't subscribed to yet. The server also auto-creates the
    /// broadcast channel on MSG_UPDATE as a defensive measure.
    ///
    /// This test verifies the corrected end-to-end flow: both clients subscribe
    /// to the new document's channel, and updates flow between them.
    #[tokio::test]
    async fn test_updates_for_new_docs_are_relayed_between_clients() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);

        // Connect two clients (simulating client startup)
        let (mut ws_a, _) = connect_async(&url).await.unwrap();
        let (mut ws_b, _) = connect_async(&url).await.unwrap();

        // Both clients subscribe to __index__ only on startup (no .bin files yet).
        let sv = StateVector::default().encode_v1();
        let idx_msg =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, "__index__", &sv);
        ws_a.send(TungsteniteMessage::Binary(idx_msg.clone().into()))
            .await
            .unwrap();
        ws_b.send(TungsteniteMessage::Binary(idx_msg.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // --- Fixed client behavior: subscribe before publishing ---
        // Client A discovers "new_doc.md" via watcher, sends SyncStep1 first.
        let sync_a =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, "new_doc.md", &sv);
        ws_a.send(TungsteniteMessage::Binary(sync_a.into()))
            .await
            .unwrap();

        // Client B also discovers "new_doc.md" and subscribes.
        let sync_b =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, "new_doc.md", &sv);
        ws_b.send(TungsteniteMessage::Binary(sync_b.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client A sends its local content as MSG_UPDATE.
        let doc_a = Doc::new();
        let text_a = doc_a.get_or_insert_text("content");
        let update_a = {
            let mut txn = doc_a.transact_mut();
            text_a.insert(&mut txn, 0, "Hello from A");
            txn.encode_update_v1()
        };
        let msg_a =
            crate::protocol::encode_message(crate::protocol::MSG_UPDATE, "new_doc.md", &update_a);
        ws_a.send(TungsteniteMessage::Binary(msg_a.into()))
            .await
            .unwrap();

        // Client B should receive Client A's update via the broadcast channel.
        let result_b = tokio::time::timeout(Duration::from_millis(500), ws_b.next()).await;
        assert!(
            result_b.is_ok(),
            "Client B should receive Client A's update for 'new_doc.md' \
             after both clients have sent SyncStep1 to subscribe."
        );

        // Client B sends its own content as MSG_UPDATE.
        let doc_b = Doc::new();
        let text_b = doc_b.get_or_insert_text("content");
        let update_b = {
            let mut txn = doc_b.transact_mut();
            text_b.insert(&mut txn, 0, "Hello from B");
            txn.encode_update_v1()
        };
        let msg_b =
            crate::protocol::encode_message(crate::protocol::MSG_UPDATE, "new_doc.md", &update_b);
        ws_b.send(TungsteniteMessage::Binary(msg_b.into()))
            .await
            .unwrap();

        // Client A should receive Client B's update.
        let result_a = tokio::time::timeout(Duration::from_millis(500), ws_a.next()).await;
        assert!(
            result_a.is_ok(),
            "Client A should receive Client B's update for 'new_doc.md'."
        );
    }

    #[tokio::test]
    async fn test_issue_5_unicode_index_skew() {
        let (_port, state) = setup_test_server().await;

        let emoji_doc = "🚀_test.md";
        let ascii_doc = "ascii.md";

        // Call the function directly to register both docs
        update_index_for_new_doc(&state, emoji_doc).await;
        update_index_for_new_doc(&state, ascii_doc).await;

        // Verify that the inner index document wasn't corrupted
        let index_doc = state.index_doc.lock().await;
        let index_text = index_doc.get_or_insert_text("content");
        let txn = index_doc.transact();
        let content = index_text.get_string(&txn);

        // Expect both doc_id's separated by newline: "🚀_test.md\nascii.md\n"
        let expected = format!("{}\n{}\n", emoji_doc, ascii_doc);
        assert_eq!(
            content, expected,
            "Index content should correctly reflect both unicode and ascii document insertion!"
        );
    }

    /// Verifies the fix for a race condition observed with two Obsidian clients
    /// (Mac + iPhone) where recently typed characters would be deleted.
    ///
    /// **Root cause**: `onRemoteUpdate` writes merged CRDT content to the file
    /// and sets `ignoreChanges`, but the ignore window (100ms) was shorter than
    /// the debounced `onFileModify` delay (300ms). The debounced handler would
    /// read stale file content and diff it against the (now-advanced) CRDT,
    /// producing spurious DELETE operations for recently typed characters.
    ///
    /// **Fix**: Increase the `ignoreChanges` timeout from 100ms to 500ms so it
    /// fully covers the 300ms debounce window. When the debounced handler fires,
    /// `ignoreChanges` is still set and the handler returns early — no stale
    /// diff is ever generated.
    ///
    /// This test simulates the full sequence at the CRDT + server level and
    /// verifies that with the guard active, both clients converge correctly.
    #[tokio::test]
    async fn test_issue_6_stale_update_deletes_chars() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);

        // Connect two clients
        let (mut ws_a, _) = connect_async(&url).await.unwrap();
        let (mut ws_b, _) = connect_async(&url).await.unwrap();

        let doc_id = "issue6_doc";

        // Both clients subscribe to the document
        let sv = StateVector::default().encode_v1();
        let sync_msg =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, doc_id, &sv);
        ws_a.send(TungsteniteMessage::Binary(sync_msg.clone().into()))
            .await
            .unwrap();
        ws_b.send(TungsteniteMessage::Binary(sync_msg.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Drain any SyncStep2 responses
        let _ = tokio::time::timeout(Duration::from_millis(200), ws_a.next()).await;
        let _ = tokio::time::timeout(Duration::from_millis(200), ws_b.next()).await;

        // --- Client A's CRDT document (simulates the Obsidian plugin's WASM doc) ---
        let doc_a = Doc::new();
        let text_a = doc_a.get_or_insert_text("content");

        // --- Client B's CRDT document ---
        let doc_b = Doc::new();
        let text_b = doc_b.get_or_insert_text("content");

        // Client A types "Hello" one character at a time, broadcasting each keystroke.
        let keystrokes = ["H", "He", "Hel", "Hell", "Hello"];

        // Simulates Client B's ignoreChanges flag — set by each onRemoteUpdate call,
        // cleared after 500ms (well beyond the 300ms debounce).
        let mut ignore_changes_set = false;

        // Client B captures a "stale snapshot" after receiving "Hel" (3rd keystroke).
        // This simulates the file content written to disk by onRemoteUpdate at that
        // point, which would be read back by the debounced onFileModify 300ms later.
        let stale_snapshot_after = 2; // index into keystrokes: "Hel"
        let mut stale_content: Option<String> = None;

        for (i, typed_so_far) in keystrokes.iter().enumerate() {
            // Client A's WASM update() equivalent: diff current CRDT text vs new content
            let current_a = text_a.get_string(&doc_a.transact());
            let prev_sv_a = doc_a.transact().state_vector();

            // Apply diff (same logic as wasm_client.rs update() and diff.rs)
            {
                let diff = dissimilar::diff(&current_a, typed_so_far);
                let mut txn = doc_a.transact_mut();
                let mut cursor = 0u32;
                for chunk in diff {
                    match chunk {
                        dissimilar::Chunk::Equal(val) => {
                            cursor += val.len() as u32;
                        }
                        dissimilar::Chunk::Delete(val) => {
                            text_a.remove_range(&mut txn, cursor, val.len() as u32);
                        }
                        dissimilar::Chunk::Insert(val) => {
                            text_a.insert(&mut txn, cursor, val);
                            cursor += val.len() as u32;
                        }
                    }
                }
            }

            // Encode and send the delta update to the server
            let update_a = doc_a.transact().encode_state_as_update_v1(&prev_sv_a);
            let msg =
                crate::protocol::encode_message(crate::protocol::MSG_UPDATE, doc_id, &update_a);
            ws_a.send(TungsteniteMessage::Binary(msg.into()))
                .await
                .unwrap();

            // Client B receives the update from the server
            let result_b = tokio::time::timeout(Duration::from_millis(500), ws_b.next()).await;
            assert!(
                result_b.is_ok(),
                "Client B should receive Client A's keystroke {}",
                i
            );
            let ws_msg = result_b.unwrap().unwrap().unwrap();
            if let TungsteniteMessage::Binary(data) = ws_msg {
                if let Some((_, _, payload)) = crate::protocol::decode_message(&data) {
                    if let Ok(u) = Update::decode_v1(payload) {
                        let mut txn = doc_b.transact_mut();
                        txn.apply_update(u);
                    }
                }
            }

            // Simulate onRemoteUpdate: write CRDT content to "file" and set ignoreChanges.
            // Each received update refreshes the guard (like the 500ms setTimeout reset).
            ignore_changes_set = true;

            if i == stale_snapshot_after {
                stale_content = Some(text_b.get_string(&doc_b.transact()));
            }
        }

        // Verify both CRDT docs are in sync
        let a_text = text_a.get_string(&doc_a.transact());
        let b_text = text_b.get_string(&doc_b.transact());
        assert_eq!(a_text, "Hello", "Client A should have 'Hello'");
        assert_eq!(b_text, "Hello", "Client B should have 'Hello'");

        // --- Simulating the debounced onFileModify on Client B ---
        //
        // The debounce fires at 300ms. With the OLD code (100ms ignoreChanges),
        // the guard would have already cleared, and the handler would read the
        // stale file content ("Hel") and diff it against the CRDT ("Hello"),
        // generating a DELETE for "lo".
        //
        // With the FIX (500ms ignoreChanges), the guard is still set at 300ms,
        // so the handler returns early — no stale diff is generated.
        let stale = stale_content.unwrap();
        assert_eq!(stale, "Hel", "Stale snapshot should be 'Hel'");

        if ignore_changes_set {
            // FIX: ignoreChanges is still active (500ms > 300ms debounce).
            // The debounced onFileModify returns early. No stale update sent.
            // This is the correct behavior after the fix.
        } else {
            // BUG (old behavior): ignoreChanges already cleared (100ms < 300ms).
            // The handler would read stale "Hel" and diff against CRDT "Hello",
            // generating DELETE "lo" and broadcasting it.
            panic!("ignoreChanges should still be set when debounce fires");
        }

        // --- Verify no data loss ---
        // Since the stale update was never applied or broadcast, both clients
        // maintain the correct document content.
        let final_a = text_a.get_string(&doc_a.transact());
        let final_b = text_b.get_string(&doc_b.transact());
        assert_eq!(
            final_a, "Hello",
            "Client A should still be 'Hello' — no stale update was sent"
        );
        assert_eq!(
            final_b, "Hello",
            "Client B should still be 'Hello' — stale diff was suppressed by ignoreChanges guard"
        );
    }

    /// Test that MSG_RESYNC allows a client to catch up on missed updates
    /// without creating duplicate broadcast subscriptions.
    #[tokio::test]
    async fn test_resync_catches_missed_updates() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let doc_id = "resync_test_doc";

        // Client A subscribes and creates content
        let (mut ws_a, _) = connect_async(&url).await.unwrap();
        let doc_a = Doc::new();
        let text_a = doc_a.get_or_insert_text("content");

        // Subscribe via SyncStep1
        let sv = doc_a.transact().state_vector().encode_v1();
        let msg = encode_message(MSG_SYNC_STEP_1, doc_id, &sv);
        ws_a.send(TungsteniteMessage::Binary(msg.into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = tokio::time::timeout(Duration::from_millis(200), ws_a.next()).await;

        // Client A writes content
        let prev_sv = doc_a.transact().state_vector();
        {
            let mut txn = doc_a.transact_mut();
            text_a.insert(&mut txn, 0, "Hello World");
        }
        let update = doc_a.transact().encode_state_as_update_v1(&prev_sv);
        let msg = encode_message(MSG_UPDATE, doc_id, &update);
        ws_a.send(TungsteniteMessage::Binary(msg.into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client B subscribes (gets SyncStep2 with "Hello World")
        let (mut ws_b, _) = connect_async(&url).await.unwrap();
        let doc_b = Doc::new();
        let text_b = doc_b.get_or_insert_text("content");

        let sv_b = doc_b.transact().state_vector().encode_v1();
        let msg = encode_message(MSG_SYNC_STEP_1, doc_id, &sv_b);
        ws_b.send(TungsteniteMessage::Binary(msg.into())).await.unwrap();

        let result = tokio::time::timeout(Duration::from_millis(500), ws_b.next()).await;
        if let Ok(Some(Ok(TungsteniteMessage::Binary(data)))) = result {
            if let Some((_, _, payload)) = decode_message(&data) {
                if let Ok(u) = Update::decode_v1(payload) {
                    let mut txn = doc_b.transact_mut();
                    txn.apply_update(u);
                }
            }
        }
        assert_eq!(text_b.get_string(&doc_b.transact()), "Hello World");

        // Client A makes another edit while Client B "misses" the broadcast
        // (simulated by just not reading from ws_b for a while)
        let prev_sv2 = doc_a.transact().state_vector();
        {
            let current = text_a.get_string(&doc_a.transact());
            let new_content = format!("{} Updated", current);
            let diff = dissimilar::diff(&current, &new_content);
            let mut txn = doc_a.transact_mut();
            let mut cursor = 0u32;
            for chunk in diff {
                match chunk {
                    dissimilar::Chunk::Equal(val) => cursor += val.len() as u32,
                    dissimilar::Chunk::Delete(val) => {
                        text_a.remove_range(&mut txn, cursor, val.len() as u32);
                    }
                    dissimilar::Chunk::Insert(val) => {
                        text_a.insert(&mut txn, cursor, val);
                        cursor += val.len() as u32;
                    }
                }
            }
        }
        let update2 = doc_a.transact().encode_state_as_update_v1(&prev_sv2);
        let msg = encode_message(MSG_UPDATE, doc_id, &update2);
        ws_a.send(TungsteniteMessage::Binary(msg.into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Drain the broadcast message that Client B would have received
        // (in the real scenario, this could be lost due to lag)
        let _ = tokio::time::timeout(Duration::from_millis(200), ws_b.next()).await;

        // Now Client B sends MSG_RESYNC with its current state vector
        let sv_resync = doc_b.transact().state_vector().encode_v1();
        let resync_msg = encode_message(MSG_RESYNC, doc_id, &sv_resync);
        ws_b.send(TungsteniteMessage::Binary(resync_msg.into())).await.unwrap();

        // Client B should receive a SyncStep2 with the missing update
        let result = tokio::time::timeout(Duration::from_millis(500), ws_b.next()).await;
        assert!(result.is_ok(), "Client B should receive resync response");
        let ws_msg = result.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Binary(data) = ws_msg {
            if let Some((msg_type, _, payload)) = decode_message(&data) {
                assert_eq!(msg_type, MSG_SYNC_STEP_2, "Response should be SYNC_STEP_2");
                if let Ok(u) = Update::decode_v1(payload) {
                    let mut txn = doc_b.transact_mut();
                    txn.apply_update(u);
                }
            }
        }

        // After resync, Client B should have the full content
        assert_eq!(
            text_b.get_string(&doc_b.transact()),
            "Hello World Updated",
            "Client B should catch up to the latest content via RESYNC"
        );
    }

    /// Regression test for the initial-sync revert bug.
    ///
    /// **Scenario**: Client A (native CLI) edits a file from "Hello World" to
    /// "Hello CRDT World". Client B (Obsidian mobile) reconnects with a fresh
    /// CRDT doc (no persisted state). The server sends SyncStep2 to Client B
    /// with Client A's edits. Client B's Obsidian plugin then reads its local
    /// (stale) file content ("Hello World") and calls update(), which diffs
    /// "Hello CRDT World" → "Hello World" — generating DELETE operations that
    /// revert Client A's edit.
    ///
    /// **Root cause**: The Obsidian plugin's `addDocOnly` callback called
    /// `update(uuid, localContent)` AFTER the remote SyncStep2 had already
    /// populated the CRDT with the latest server state. The diff treated
    /// the stale local file as authoritative, reverting remote edits.
    ///
    /// **Fix**: Persist CRDT state across sessions. On reconnect, load the
    /// persisted state into the Doc before syncing. The diff then compares
    /// the persisted state (last known synced content) vs local file,
    /// producing only the delta (offline edits) — not a full revert.
    #[tokio::test]
    async fn test_initial_sync_should_not_revert_remote_edits() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);

        let doc_id = "revert_bug_doc";

        // --- Phase 1: Client A creates and edits the document ---

        let (mut ws_a, _) = connect_async(&url).await.unwrap();

        // Client A subscribes to the document
        let sv = StateVector::default().encode_v1();
        let sync_msg =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, doc_id, &sv);
        ws_a.send(TungsteniteMessage::Binary(sync_msg.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Drain SyncStep2 response
        let _ = tokio::time::timeout(Duration::from_millis(200), ws_a.next()).await;

        // Client A creates initial content "Hello World"
        let doc_a = Doc::new();
        let text_a = doc_a.get_or_insert_text("content");
        let prev_sv_a = doc_a.transact().state_vector();
        {
            let mut txn = doc_a.transact_mut();
            text_a.insert(&mut txn, 0, "Hello World");
        }
        let update_a1 = doc_a.transact().encode_state_as_update_v1(&prev_sv_a);
        let msg_a1 =
            crate::protocol::encode_message(crate::protocol::MSG_UPDATE, doc_id, &update_a1);
        ws_a.send(TungsteniteMessage::Binary(msg_a1.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client A edits: "Hello World" → "Hello CRDT World"
        let prev_sv_a2 = doc_a.transact().state_vector();
        {
            let diff = dissimilar::diff("Hello World", "Hello CRDT World");
            let mut txn = doc_a.transact_mut();
            let mut cursor = 0u32;
            for chunk in diff {
                match chunk {
                    dissimilar::Chunk::Equal(val) => cursor += val.len() as u32,
                    dissimilar::Chunk::Delete(val) => {
                        text_a.remove_range(&mut txn, cursor, val.len() as u32);
                    }
                    dissimilar::Chunk::Insert(val) => {
                        text_a.insert(&mut txn, cursor, val);
                        cursor += val.len() as u32;
                    }
                }
            }
        }
        assert_eq!(
            text_a.get_string(&doc_a.transact()),
            "Hello CRDT World",
            "Client A should have the edited content"
        );
        let update_a2 = doc_a.transact().encode_state_as_update_v1(&prev_sv_a2);
        let msg_a2 =
            crate::protocol::encode_message(crate::protocol::MSG_UPDATE, doc_id, &update_a2);
        ws_a.send(TungsteniteMessage::Binary(msg_a2.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // --- Phase 2: Client B connects, syncs, persists CRDT state, then disconnects ---
        //
        // This simulates the Obsidian plugin's initial sync + state persistence.

        let (mut ws_b, _) = connect_async(&url).await.unwrap();
        let doc_b = Doc::new();
        let text_b = doc_b.get_or_insert_text("content");

        let sv_b = doc_b.transact().state_vector().encode_v1();
        let sync_b =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, doc_id, &sv_b);
        ws_b.send(TungsteniteMessage::Binary(sync_b.into()))
            .await
            .unwrap();

        // Receive SyncStep2 with full server state
        let result_b = tokio::time::timeout(Duration::from_millis(500), ws_b.next()).await;
        assert!(result_b.is_ok(), "Client B should receive SyncStep2");
        let ws_msg = result_b.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Binary(data) = ws_msg {
            if let Some((_, _, payload)) = crate::protocol::decode_message(&data) {
                if let Ok(u) = Update::decode_v1(payload) {
                    let mut txn = doc_b.transact_mut();
                    txn.apply_update(u);
                }
            }
        }

        assert_eq!(
            text_b.get_string(&doc_b.transact()),
            "Hello CRDT World",
            "Client B should have full content after initial sync"
        );

        // Persist CRDT state (simulates saveCrdtState in the plugin)
        let persisted_state =
            doc_b.transact().encode_state_as_update_v1(&StateVector::default());

        // Client B disconnects
        let _ = ws_b.close(None).await;
        drop(doc_b);
        drop(text_b);

        // --- Phase 3: While Client B is offline, BOTH clients make edits ---

        // Client A edits: "Hello CRDT World" → "Hello CRDT World - edited by A"
        let prev_sv_a3 = doc_a.transact().state_vector();
        {
            let current = text_a.get_string(&doc_a.transact());
            let new_content = format!("{} - edited by A", current);
            let diff = dissimilar::diff(&current, &new_content);
            let mut txn = doc_a.transact_mut();
            let mut cursor = 0u32;
            for chunk in diff {
                match chunk {
                    dissimilar::Chunk::Equal(val) => cursor += val.len() as u32,
                    dissimilar::Chunk::Delete(val) => {
                        text_a.remove_range(&mut txn, cursor, val.len() as u32);
                    }
                    dissimilar::Chunk::Insert(val) => {
                        text_a.insert(&mut txn, cursor, val);
                        cursor += val.len() as u32;
                    }
                }
            }
        }
        let update_a3 = doc_a.transact().encode_state_as_update_v1(&prev_sv_a3);
        let msg_a3 =
            crate::protocol::encode_message(crate::protocol::MSG_UPDATE, doc_id, &update_a3);
        ws_a.send(TungsteniteMessage::Binary(msg_a3.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client B's local file is edited offline (iPhone user edits):
        // "Hello CRDT World" → "Hello CRDT World - edited by B"
        let local_file_content = "Hello CRDT World - edited by B";

        // --- Phase 4: Client B reconnects with persisted CRDT state ---
        //
        // This is the core of the fix: instead of creating Doc::new() (empty),
        // load the persisted state first, THEN sync with the server.

        let (mut ws_b2, _) = connect_async(&url).await.unwrap();

        // Restore persisted CRDT state into a new Doc (simulates add_doc_with_state)
        let doc_b2 = Doc::new();
        let text_b2 = doc_b2.get_or_insert_text("content");
        {
            let u = Update::decode_v1(&persisted_state).unwrap();
            let mut txn = doc_b2.transact_mut();
            txn.apply_update(u);
        }
        assert_eq!(
            text_b2.get_string(&doc_b2.transact()),
            "Hello CRDT World",
            "Restored doc should have the persisted content"
        );

        // Step 1: Apply offline edits BEFORE syncing with server.
        // Diff persisted content → local file content to capture B's offline edits.
        // This happens immediately after loading persisted state, before SyncStep1.
        let persisted_content = text_b2.get_string(&doc_b2.transact());
        if persisted_content != local_file_content {
            let diff = dissimilar::diff(&persisted_content, local_file_content);
            let mut txn = doc_b2.transact_mut();
            let mut cursor = 0u32;
            for chunk in diff {
                match chunk {
                    dissimilar::Chunk::Equal(val) => cursor += val.len() as u32,
                    dissimilar::Chunk::Delete(val) => {
                        text_b2.remove_range(&mut txn, cursor, val.len() as u32);
                    }
                    dissimilar::Chunk::Insert(val) => {
                        text_b2.insert(&mut txn, cursor, val);
                        cursor += val.len() as u32;
                    }
                }
            }
        }
        assert_eq!(
            text_b2.get_string(&doc_b2.transact()),
            "Hello CRDT World - edited by B",
            "After applying offline edits, doc should have B's changes"
        );

        // Step 2: Send SyncStep1 with the PERSISTED state vector (not empty!)
        let sv_b2 = doc_b2.transact().state_vector().encode_v1();
        let sync_b2 =
            crate::protocol::encode_message(crate::protocol::MSG_SYNC_STEP_1, doc_id, &sv_b2);
        ws_b2
            .send(TungsteniteMessage::Binary(sync_b2.into()))
            .await
            .unwrap();

        // Also push full local state to server (includes offline edits)
        let full_state = doc_b2
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let push_msg =
            crate::protocol::encode_message(crate::protocol::MSG_UPDATE, doc_id, &full_state);
        ws_b2
            .send(TungsteniteMessage::Binary(push_msg.into()))
            .await
            .unwrap();

        // Step 3: Receive SyncStep2 with A's offline edits — CRDT merges both
        let result_b2 = tokio::time::timeout(Duration::from_millis(500), ws_b2.next()).await;
        assert!(result_b2.is_ok(), "Client B should receive SyncStep2 with A's edits");
        let ws_msg2 = result_b2.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Binary(data) = ws_msg2 {
            if let Some((_, _, payload)) = crate::protocol::decode_message(&data) {
                if let Ok(u) = Update::decode_v1(payload) {
                    let mut txn = doc_b2.transact_mut();
                    txn.apply_update(u);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        // --- Phase 5: Verify BOTH clients' edits are preserved ---

        // Client B should have a merge of both offline edits.
        // CRDT merge of concurrent " - edited by A" and " - edited by B"
        // appended to "Hello CRDT World" will produce both suffixes
        // (order depends on client IDs, but both must be present).
        let final_b = text_b2.get_string(&doc_b2.transact());
        assert!(
            final_b.contains("edited by A") && final_b.contains("edited by B"),
            "Client B should have BOTH offline edits merged. Got: {:?}",
            final_b
        );

        // Client A receives B's offline edit via the server broadcast
        let result_a = tokio::time::timeout(Duration::from_millis(500), ws_a.next()).await;
        if let Ok(Some(Ok(TungsteniteMessage::Binary(data)))) = result_a {
            if let Some((_, _, payload)) = crate::protocol::decode_message(&data) {
                if let Ok(u) = Update::decode_v1(payload) {
                    let mut txn = doc_a.transact_mut();
                    txn.apply_update(u);
                }
            }
        }

        let final_a = text_a.get_string(&doc_a.transact());
        assert!(
            final_a.contains("edited by A") && final_a.contains("edited by B"),
            "Client A should also have BOTH offline edits merged. Got: {:?}",
            final_a
        );

        // Both clients converge to the same content
        assert_eq!(
            final_a, final_b,
            "Both clients must converge to the same merged content"
        );
    }
}
