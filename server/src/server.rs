use crate::db::Db;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::{broadcast, mpsc, RwLock};
use yrs::{updates::decoder::Decode, StateVector};

// Message types
use syncline::protocol::{MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE};

#[derive(Clone)]
struct AppState {
    db: Db,
    // Map doc_id -> broadcast sender
    channels: Arc<RwLock<HashMap<String, broadcast::Sender<Vec<u8>>>>>,
}

pub async fn run_server(db: Db, port: u16) -> anyhow::Result<()> {
    let state = AppState {
        db,
        channels: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/sync/:doc_id", get(ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local_addr = listener.local_addr()?;
    println!("Server listening on {}", local_addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(doc_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, doc_id, state))
}

async fn handle_socket(socket: WebSocket, doc_id: String, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Channel for messages to be sent to this client
    let (tx_socket, mut rx_socket) = mpsc::channel::<Vec<u8>>(100);

    // Subscribe to broadcast channel
    let mut broadcast_rx = {
        let mut channels = state.channels.write().await;
        // Ensure channel exists
        let tx = channels.entry(doc_id.clone()).or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(100);
            tx
        });
        tx.subscribe()
    };

    // Task 1: Forward messages from MPSC to WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(data) = rx_socket.recv().await {
            if sender.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // Task 2: Forward broadcast updates to MPSC
    let tx_socket_clone = tx_socket.clone();
    let broadcast_task = tokio::spawn(async move {
        while let Ok(msg) = broadcast_rx.recv().await {
            // Prepend MSG_UPDATE type
            let mut data = vec![MSG_UPDATE];
            data.extend(msg);
            if tx_socket_clone.send(data).await.is_err() {
                break;
            }
        }
    });

    // Task 3: Receive from WebSocket
    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Binary(data) = msg {
            if data.is_empty() {
                continue;
            }
            let msg_type = data[0];
            let payload = &data[1..];

            match msg_type {
                MSG_SYNC_STEP_1 => {
                    // Client sent StateVector, asking for sync
                    if let Ok(sv) = StateVector::decode_v1(payload) {
                        match state.db.get_all_updates_since(&doc_id, &sv).await {
                            Ok(update) => {
                                // Send back accumulated update
                                if !update.is_empty() {
                                    let mut resp = vec![MSG_SYNC_STEP_2];
                                    resp.extend(update);
                                    let _ = tx_socket.send(resp).await;
                                }
                            }
                            Err(e) => eprintln!("DB Error: {}", e),
                        }
                    }
                }
                MSG_UPDATE => {
                    // Client sent an update
                    // 1. Save to DB
                    if let Err(e) = state.db.save_update(&doc_id, payload).await {
                        eprintln!("DB Save Error: {}", e);
                    } else {
                        // 2. Broadcast to other clients
                        let channels = state.channels.read().await;
                        if let Some(tx) = channels.get(&doc_id) {
                            // We construct the raw payload to broadcast (without prefix here, prefix added by receiver)
                            let _ = tx.send(payload.to_vec());
                        }
                    }
                }
                _ => {}
            }
        } else if let Message::Close(_) = msg {
            break;
        }
    }

    send_task.abort();
    broadcast_task.abort();
}
