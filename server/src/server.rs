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

use syncline::protocol::{decode_message, encode_message, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE};

#[derive(Clone)]
struct AppState {
    db: Db,
    channels: Arc<RwLock<HashMap<String, broadcast::Sender<Vec<u8>>>>>,
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

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let (tx_socket, mut rx_socket) = mpsc::channel::<Vec<u8>>(100);

    // Track broadcast receivers for this client (doc_id -> receiver)
    let broadcast_receivers = Arc::new(RwLock::new(HashMap::<String, broadcast::Receiver<Vec<u8>>>::new()));

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
    let broadcast_receivers_clone = broadcast_receivers.clone();
    let state_clone = state.clone();
    
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Binary(data) = msg {
                if let Some((msg_type, doc_id, payload)) = decode_message(&data) {
match msg_type {
                        MSG_SYNC_STEP_1 => {
                            log::info!("Received SYNC_STEP_1 for doc: {}", doc_id);
                            // Subscribe to broadcast channel for this doc
                            let rx = {
                                let mut channels = state_clone.channels.write().await;
                                let tx = channels.entry(doc_id.to_string()).or_insert_with(|| {
                                    let (tx, _rx) = broadcast::channel(100);
                                    tx
                                });
                                tx.subscribe()
                            };
                            
                            // Store receiver for broadcast task
                            broadcast_receivers_clone.write().await.insert(doc_id.to_string(), rx);

                            // Send back the current state
                            if let Ok(sv) = StateVector::decode_v1(payload) {
                                match state_clone.db.get_all_updates_since(doc_id, &sv).await {
                                    Ok(update) if !update.is_empty() => {
                                        log::info!("Sending SYNC_STEP_2 for doc {} with {} bytes", doc_id, update.len());
                                        let resp = encode_message(MSG_SYNC_STEP_2, doc_id, &update);
                                        let _ = tx_socket_clone.send(resp).await;
                                    }
                                    Ok(_) => {
                                        log::info!("No updates to send for doc {}", doc_id);
                                    }
                                    Err(e) => log::error!("DB Error: {}", e),
                                }
            }
                        }
                        MSG_UPDATE => {
                            log::info!("Received UPDATE for doc: {} ({} bytes)", doc_id, payload.len());
                            if let Err(e) = state_clone.db.save_update(doc_id, payload).await {
                                log::error!("DB Save Error: {}", e);
                            } else {
                                log::info!("Saved update for doc {}, broadcasting", doc_id);
                                // Broadcast to other clients
                                let channels = state_clone.channels.read().await;
                                if let Some(tx) = channels.get(doc_id) {
                                    let _ = tx.send(payload.to_vec());
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

    // Task 3: Forward broadcasts to client
    let tx_socket_broadcast = tx_socket.clone();
    let broadcast_task = tokio::spawn(async move {
        loop {
            // Wait for any broadcast receiver to have data
            let mut receivers = broadcast_receivers.write().await;
            if receivers.is_empty() {
                drop(receivers);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            
            // Try to receive from each channel
            let mut to_remove = Vec::new();
            for (doc_id, rx) in receivers.iter_mut() {
                match rx.try_recv() {
                    Ok(payload) => {
                        let msg = encode_message(MSG_UPDATE, doc_id, &payload);
                        let _ = tx_socket_broadcast.send(msg).await;
                    }
                    Err(broadcast::error::TryRecvError::Closed) => {
                        to_remove.push(doc_id.clone());
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {}
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                }
            }
            
            for doc_id in to_remove {
                receivers.remove(&doc_id);
            }
            
            drop(receivers);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });

    recv_task.await.ok();
    send_task.abort();
    broadcast_task.abort();
}