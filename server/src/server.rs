use crate::db::Db;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::{broadcast, mpsc, RwLock};
use yrs::{updates::decoder::Decode, StateVector};

use syncline::protocol::{
    decode_message, encode_message, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE,
};

#[derive(Clone)]
struct AppState {
    db: Db,
    channels: Arc<RwLock<HashMap<String, broadcast::Sender<(Vec<u8>, uuid::Uuid)>>>>,
}

pub async fn run_server(db: Db, port: u16) -> anyhow::Result<()> {
    let state = AppState {
        db,
        channels: Arc::new(RwLock::new(HashMap::new())),
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
                            log::info!("Received SYNC_STEP_1 for doc: {}", doc_id);

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

                            // Spawn an event-driven forwarding task for this doc.
                            // It exits automatically when the outgoing sender is closed
                            // (i.e. when this WebSocket connection ends).
                            let tx_fwd = tx_socket_clone.clone();
                            let doc_id_str = doc_id.to_string();
                            tokio::spawn(async move {
                                let mut rx = rx;
                                loop {
                                    tokio::select! {
                                        _ = tx_fwd.closed() => break,
                                        res = rx.recv() => {
                                            match res {
                                                Ok((payload, sender_id)) => {
                                                    if sender_id == connection_id {
                                                        continue;
                                                    }
                                                    let msg =
                                                        encode_message(MSG_UPDATE, &doc_id_str, &payload);
                                                    if tx_fwd.send(msg).is_err() {
                                                        // Outgoing channel closed — connection gone.
                                                        break;
                                                    }
                                                }
                                                Err(broadcast::error::RecvError::Closed) => break,
                                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                                    // Even with a large buffer this can happen under
                                                    // extreme load.  Log it; the receiver is
                                                    // automatically advanced to the oldest available
                                                    // message, so no explicit action is needed.
                                                    log::warn!(
                                                        "Broadcast receiver lagged by {} messages for doc {}",
                                                        n, doc_id_str
                                                    );
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
                                        log::info!(
                                            "Sending SYNC_STEP_2 for doc {} with {} bytes",
                                            doc_id,
                                            update.len()
                                        );
                                        let resp = encode_message(MSG_SYNC_STEP_2, doc_id, &update);
                                        let _ = tx_socket_clone.send(resp);
                                    }
                                    Ok(_) => {
                                        log::info!("No updates to send for doc {}", doc_id);
                                    }
                                    Err(e) => log::error!("DB Error: {}", e),
                                }
                            }
                        }
                        MSG_UPDATE => {
                            let db = state_clone.db.clone();
                            let payload_clone = payload.to_vec();
                            let doc_id_clone = doc_id.to_string();
                            tokio::spawn(async move {
                                if let Err(e) = db.save_update(&doc_id_clone, &payload_clone).await
                                {
                                    log::error!("DB Save Error: {}", e);
                                }
                            });

                            // Auto-create the channel if it doesn't exist yet. This handles
                            // the race where a client sends MSG_UPDATE before any SyncStep1
                            // has been received for this doc_id.
                            let mut channels = state_clone.channels.write().await;
                            let tx = channels
                                .entry(doc_id.to_string())
                                .or_insert_with(|| broadcast::channel(65_536).0);
                            let _ = tx.send((payload.to_vec(), connection_id));
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
    use yrs::{Doc, Text, Transact};

    async fn setup_test_server() -> (u16, AppState) {
        let db = Db::new("sqlite::memory:").await.unwrap();
        let state = AppState {
            db,
            channels: Arc::new(RwLock::new(HashMap::new())),
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
        let msg = syncline::protocol::encode_message(
            syncline::protocol::MSG_SYNC_STEP_1,
            doc_id,
            &payload,
        );

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
        let msg = syncline::protocol::encode_message(
            syncline::protocol::MSG_SYNC_STEP_1,
            doc_id,
            &payload,
        );

        ws_stream
            .send(TungsteniteMessage::Binary(msg.into()))
            .await
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
            syncline::protocol::encode_message(syncline::protocol::MSG_UPDATE, doc_id, &update);
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
        let idx_msg = syncline::protocol::encode_message(
            syncline::protocol::MSG_SYNC_STEP_1,
            "__index__",
            &sv,
        );
        ws_a.send(TungsteniteMessage::Binary(idx_msg.clone().into()))
            .await
            .unwrap();
        ws_b.send(TungsteniteMessage::Binary(idx_msg.into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // --- Fixed client behavior: subscribe before publishing ---
        // Client A discovers "new_doc.md" via watcher, sends SyncStep1 first.
        let sync_a = syncline::protocol::encode_message(
            syncline::protocol::MSG_SYNC_STEP_1,
            "new_doc.md",
            &sv,
        );
        ws_a.send(TungsteniteMessage::Binary(sync_a.into()))
            .await
            .unwrap();

        // Client B also discovers "new_doc.md" and subscribes.
        let sync_b = syncline::protocol::encode_message(
            syncline::protocol::MSG_SYNC_STEP_1,
            "new_doc.md",
            &sv,
        );
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
        let msg_a = syncline::protocol::encode_message(
            syncline::protocol::MSG_UPDATE,
            "new_doc.md",
            &update_a,
        );
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
        let msg_b = syncline::protocol::encode_message(
            syncline::protocol::MSG_UPDATE,
            "new_doc.md",
            &update_b,
        );
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
}
