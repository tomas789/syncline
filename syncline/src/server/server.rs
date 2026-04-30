//! v1 manifest-sync server.
//!
//! The server owns one long-lived in-memory [`Manifest`] (hydrated from
//! the DB on startup) plus a set of per-doc broadcast channels. Clients
//! speak the v1 wire protocol (see `protocol.rs`):
//!
//! - first frame must be [`MSG_VERSION`] with major/minor = 1/0
//! - manifest sync is driven through [`MSG_MANIFEST_SYNC`] /
//!   [`MSG_MANIFEST_VERIFY`] frames targeting [`MANIFEST_DOC_ID`]
//! - text content subdocs sync through standard
//!   [`MSG_SYNC_STEP_1`] / [`MSG_SYNC_STEP_2`] / [`MSG_UPDATE`] frames
//!   with `doc_id = "content:<node-hex>"`
//! - binaries continue through [`MSG_BLOB_UPDATE`] / [`MSG_BLOB_REQUEST`]
//!
//! A v0 client that speaks a pre-manifest protocol will either fail the
//! version handshake (if it sends no MSG_VERSION) or send messages that
//! don't match a known v1 type — both paths close the connection with
//! an explanatory log line. We do not ship a bespoke "upgrade required"
//! frame because v0 clients would interpret any unknown payload as
//! garbage anyway; closing the socket is the least ambiguous signal.

use crate::protocol::{
    MANIFEST_DOC_ID, MSG_BLOB_REQUEST, MSG_BLOB_UPDATE, MSG_MANIFEST_SYNC,
    MSG_MANIFEST_VERIFY, MSG_SERVER_STATS, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE,
    MSG_VERSION, V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR, decode_message, encode_message,
};
use crate::server::db::Db;
use crate::server::migration::migrate_server_db;
use crate::v1::ids::Lamport;
use crate::v1::manifest::Manifest;
use crate::v1::sync::{
    decode_version_handshake, encode_version_handshake, handle_manifest_payload,
    handle_verify_payload, manifest_step1_payload, split_manifest_payload,
};
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
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::{Mutex as AsyncMutex, RwLock, broadcast, mpsc};
use yrs::{StateVector, updates::decoder::Decode};

type ChannelMap = Arc<RwLock<HashMap<String, broadcast::Sender<(Vec<u8>, uuid::Uuid)>>>>;

// Per-doc broadcast channels are created lazily, one per subscribed doc.
// `tokio::sync::broadcast::channel(N)` pre-allocates a ring buffer of N
// slots up front, so this constant is a fixed memory cost multiplied by
// every subscribed doc — on a 1000-doc vault, 65_536 → ~3 GB of empty
// ring buffer alone, which OOMs the server during initial fan-out. The
// buffer only needs to absorb burst lag for a single forwarding task per
// subscriber, which catches up immediately, so a small cap is plenty.
const PER_DOC_BROADCAST_CAP: usize = 64;

#[derive(Clone)]
struct AppState {
    db: Db,
    channels: ChannelMap,
    /// The server's authoritative manifest. Locked for mutation while
    /// applying an incoming MANIFEST_STEP_2/UPDATE; held only briefly
    /// for STEP_1 responses (state-vector read + update encoding).
    manifest: Arc<AsyncMutex<Manifest>>,
    /// Wall-clock instant when this server process started. Used by
    /// the `MSG_SERVER_STATS` handler (#65 phase 5) to report uptime
    /// — no cross-restart accumulation, just "how long has this
    /// process been up", which is the user-visible signal.
    started_at: std::time::Instant,
}

pub async fn run_server(db: Db, port: u16) -> anyhow::Result<()> {
    // Phase 3.2: migrate the DB if it's still v0 — idempotent if
    // already migrated.
    let report = migrate_server_db(&db).await?;
    if !report.already_migrated {
        tracing::info!(
            "Migrated server DB to v1: {} text + {} binary + {} skipped (actor {})",
            report.text_docs,
            report.binary_docs,
            report.skipped,
            report.actor_id.to_string_hyphenated(),
        );
        for w in &report.warnings {
            tracing::warn!("migration warning: {}", w);
        }
    } else {
        tracing::info!(
            "Server DB already at v1 (actor {})",
            report.actor_id.to_string_hyphenated()
        );
    }

    // Hydrate the in-memory manifest from DB rows.
    let manifest = hydrate_manifest(&db, report.actor_id).await?;

    let state = AppState {
        db,
        channels: Arc::new(RwLock::new(HashMap::new())),
        manifest: Arc::new(AsyncMutex::new(manifest)),
        started_at: std::time::Instant::now(),
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

async fn hydrate_manifest(db: &Db, actor: crate::v1::ids::ActorId) -> anyhow::Result<Manifest> {
    let updates = db.load_doc_updates(MANIFEST_DOC_ID).await.unwrap_or_default();
    if updates.is_empty() {
        return Ok(Manifest::new(actor));
    }
    // Merge every stored update into a fresh manifest. Yrs updates are
    // idempotent and commutative, so order doesn't matter.
    let mut m = Manifest::new(actor);
    for u in updates {
        if let Err(e) = m.apply_update(&u) {
            tracing::warn!("skipping corrupt manifest update: {}", e);
        }
    }
    let _ = Lamport::ZERO; // silence unused import if the type lives in ids
    Ok(m)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let connection_id = uuid::Uuid::new_v4();
    let (mut sender, mut receiver) = socket.split();

    let (tx_socket, mut rx_socket) = mpsc::unbounded_channel::<Vec<u8>>();

    let send_task = tokio::spawn(async move {
        while let Some(data) = rx_socket.recv().await {
            if sender.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    let tx_out = tx_socket.clone();
    let state_for_recv = state.clone();

    let recv_task = tokio::spawn(async move {
        // -----------------------------------------------------------------
        // Step 1 — version handshake. First frame must be MSG_VERSION.
        // -----------------------------------------------------------------
        let first = match receiver.next().await {
            Some(Ok(Message::Binary(data))) => data,
            Some(Ok(Message::Close(_))) => return,
            _ => {
                tracing::warn!(
                    conn = %connection_id,
                    "closing: first frame was not a binary WebSocket message"
                );
                return;
            }
        };
        let Some((msg_type, doc_id, payload)) = decode_message(&first) else {
            tracing::warn!(conn = %connection_id, "closing: malformed first frame");
            return;
        };
        if msg_type != MSG_VERSION {
            tracing::warn!(
                conn = %connection_id,
                msg_type,
                doc_id,
                "closing: first frame must be MSG_VERSION (client speaks v0 or wrong protocol)"
            );
            return;
        }
        let Some((major, minor)) = decode_version_handshake(payload) else {
            tracing::warn!(conn = %connection_id, "closing: malformed version handshake");
            return;
        };
        if major != V1_PROTOCOL_MAJOR {
            tracing::warn!(
                conn = %connection_id,
                remote = format!("{}.{}", major, minor),
                "closing: incompatible protocol version — expected {}.{}",
                V1_PROTOCOL_MAJOR,
                V1_PROTOCOL_MINOR
            );
            return;
        }
        // Echo our version back so the client can confirm the server
        // is v1 too.
        let _ = tx_out.send(encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        ));
        tracing::info!(conn = %connection_id, "v1 handshake OK (peer {}.{})", major, minor);

        // -----------------------------------------------------------------
        // Step 2 — message loop.
        // -----------------------------------------------------------------
        loop {
            let msg = match receiver.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    // Critical — without this, an oversized inbound
                    // frame, a TLS error, or any other protocol-level
                    // teardown by `tokio-tungstenite` disappears from
                    // the server log entirely; the client just sees a
                    // bare `Connection reset by peer`. See #59 for the
                    // class of failure this log line catches.
                    tracing::warn!(
                        conn = %connection_id,
                        error = %e,
                        "websocket recv error: tearing down session"
                    );
                    break;
                }
                None => {
                    tracing::debug!(conn = %connection_id, "websocket stream ended");
                    break;
                }
            };
            let data = match msg {
                Message::Binary(b) => b,
                Message::Close(frame) => {
                    tracing::debug!(
                        conn = %connection_id,
                        frame = ?frame,
                        "peer closed websocket"
                    );
                    break;
                }
                _ => continue,
            };
            let Some((msg_type, doc_id, payload)) = decode_message(&data) else {
                tracing::debug!(conn = %connection_id, "skipping malformed frame");
                continue;
            };
            match msg_type {
                MSG_MANIFEST_SYNC if doc_id == MANIFEST_DOC_ID => {
                    handle_manifest_sync(
                        &state_for_recv,
                        connection_id,
                        &tx_out,
                        payload,
                    )
                    .await;
                }
                MSG_MANIFEST_VERIFY if doc_id == MANIFEST_DOC_ID => {
                    handle_manifest_verify(&state_for_recv, &tx_out, payload).await;
                }
                MSG_SERVER_STATS if doc_id == MANIFEST_DOC_ID => {
                    handle_server_stats(&state_for_recv, &tx_out).await;
                }
                MSG_SYNC_STEP_1 if doc_id.starts_with("content:") => {
                    handle_content_step1(
                        &state_for_recv,
                        connection_id,
                        &tx_out,
                        doc_id,
                        payload,
                    )
                    .await;
                }
                MSG_SYNC_STEP_2 | MSG_UPDATE if doc_id.starts_with("content:") => {
                    // Persist the raw yrs update and broadcast. STEP_2
                    // and UPDATE are semantically identical on the
                    // wire: the payload is always a yrs update.
                    handle_content_update(
                        &state_for_recv,
                        connection_id,
                        doc_id,
                        payload,
                    )
                    .await;
                }
                MSG_BLOB_UPDATE => {
                    handle_blob_update(
                        &state_for_recv,
                        connection_id,
                        doc_id,
                        payload,
                    )
                    .await;
                }
                MSG_BLOB_REQUEST => {
                    handle_blob_request(&state_for_recv, &tx_out, doc_id, payload).await;
                }
                other => {
                    tracing::debug!(
                        conn = %connection_id,
                        msg_type = other,
                        doc_id,
                        "ignoring frame with unexpected msg_type / doc_id"
                    );
                }
            }
        }
        tracing::debug!(conn = %connection_id, "receive loop ended");
    });

    recv_task.await.ok();
    send_task.abort();
}

// ---------------------------------------------------------------------------
// Manifest handlers
// ---------------------------------------------------------------------------

async fn handle_manifest_sync(
    state: &AppState,
    conn: uuid::Uuid,
    tx_out: &mpsc::UnboundedSender<Vec<u8>>,
    payload: &[u8],
) {
    let Some((sub_type, _inner)) = split_manifest_payload(payload) else {
        return;
    };

    // Ensure there's a broadcast channel for the manifest, and that
    // this connection is subscribed. Subscribe-on-first-frame keeps
    // the channel alive for every connected v1 client.
    ensure_subscribed(state, MANIFEST_DOC_ID.to_string(), conn, tx_out).await;

    let mut manifest = state.manifest.lock().await;
    match handle_manifest_payload(&mut manifest, payload) {
        Ok(Some(response_payload)) => {
            // STEP_1 arrived — respond with STEP_2 to just this client.
            // No persistence, no broadcast (STEP_2 is peer-directed).
            let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &response_payload);
            let _ = tx_out.send(frame);

            // Second half of the bidirectional Yrs sync handshake: send
            // our own STEP_1 so the client replies with a STEP_2
            // carrying whatever *we* are missing. Without this follow-up
            // the exchange is one-directional — a populated client
            // connecting to an empty server (e.g. post-`syncline migrate`)
            // would never push its state upstream.
            let our_step1 = manifest_step1_payload(&manifest);
            let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &our_step1);
            let _ = tx_out.send(frame);
        }
        Ok(None) => {
            // STEP_2 / UPDATE applied to the server's manifest. Persist
            // the *inner* yrs update (drop the 1-byte sub-type prefix)
            // so another server restart rehydrates the same state.
            use crate::protocol::MANIFEST_UPDATE;
            if sub_type == crate::protocol::MANIFEST_STEP_2
                || sub_type == MANIFEST_UPDATE
            {
                // payload[0] is sub-type, rest is the yrs update.
                let inner = &payload[1..];
                if let Err(e) = state.db.save_update(MANIFEST_DOC_ID, inner).await {
                    tracing::error!("persist manifest update failed: {}", e);
                    return;
                }
                // Rebroadcast as MANIFEST_UPDATE (STEP_2 is peer-
                // directed; UPDATE is the broadcast form).
                let mut rebroadcast = Vec::with_capacity(1 + inner.len());
                rebroadcast.push(MANIFEST_UPDATE);
                rebroadcast.extend_from_slice(inner);
                let framed =
                    encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &rebroadcast);
                if let Some(tx) = state.channels.read().await.get(MANIFEST_DOC_ID) {
                    let _ = tx.send((framed, conn));
                }
            }
        }
        Err(e) => {
            tracing::warn!("manifest sync payload rejected: {}", e);
        }
    }
}

async fn handle_manifest_verify(
    state: &AppState,
    tx_out: &mpsc::UnboundedSender<Vec<u8>>,
    payload: &[u8],
) {
    let manifest = state.manifest.lock().await;
    match handle_verify_payload(&manifest, payload) {
        Ok(Some(step1_payload)) => {
            let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &step1_payload);
            let _ = tx_out.send(frame);
        }
        Ok(None) => {
            // Converged.
        }
        Err(e) => {
            tracing::warn!("verify payload rejected: {}", e);
        }
    }
}

/// Build and reply with a `MSG_SERVER_STATS` JSON payload (#65 §4).
///
/// Backwards-compatible with old clients: they send no request, the
/// server sends no unsolicited reply — the request/response shape
/// is strictly client-initiated. New clients send an empty payload
/// and consume the JSON.
async fn handle_server_stats(
    state: &AppState,
    tx_out: &mpsc::UnboundedSender<Vec<u8>>,
) {
    // `connected_clients` is the per-vault MANIFEST broadcast channel's
    // receiver count — i.e. how many WS connections currently subscribe
    // to manifest updates. That's exactly the count the user cares
    // about ("how many other clients have my vault open").
    let connected_clients = {
        let channels = state.channels.read().await;
        channels
            .get(MANIFEST_DOC_ID)
            .map(|tx| tx.receiver_count())
            .unwrap_or(0) as u32
    };
    // Manifest node count — live entries. Cheap; the manifest is
    // already in memory.
    let (manifest_node_count, manifest_total_size) = {
        let manifest = state.manifest.lock().await;
        let live = manifest.live_entries();
        let total: u64 = live.iter().map(|e| e.size).sum();
        (live.len() as u32, total)
    };
    // Blob counts — single SQL aggregate. Cheap on any reasonable
    // vault because the blobs table tracks `size` as a separate
    // column.
    let (blob_count, blob_total_bytes) = state
        .db
        .blob_summary()
        .await
        .unwrap_or((0, 0));

    let uptime_secs = state.started_at.elapsed().as_secs();
    let payload = serde_json::json!({
        "server_version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": uptime_secs,
        "connected_clients": connected_clients,
        "manifest_node_count": manifest_node_count,
        "manifest_total_bytes": manifest_total_size,
        "blob_count": blob_count,
        "blob_total_bytes": blob_total_bytes,
    });
    let body = match serde_json::to_vec(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("server_stats serialise: {}", e);
            return;
        }
    };
    let frame = encode_message(MSG_SERVER_STATS, MANIFEST_DOC_ID, &body);
    let _ = tx_out.send(frame);
}

// ---------------------------------------------------------------------------
// Content subdoc handlers
// ---------------------------------------------------------------------------

async fn handle_content_step1(
    state: &AppState,
    conn: uuid::Uuid,
    tx_out: &mpsc::UnboundedSender<Vec<u8>>,
    doc_id: &str,
    payload: &[u8],
) {
    ensure_subscribed(state, doc_id.to_string(), conn, tx_out).await;

    let Ok(sv) = StateVector::decode_v1(payload) else {
        return;
    };

    // 1) Reply with what we have past the client's SV (existing behaviour).
    match state.db.get_all_updates_since(doc_id, &sv).await {
        Ok(update) if !update.is_empty() => {
            let frame = encode_message(MSG_SYNC_STEP_2, doc_id, &update);
            let _ = tx_out.send(frame);
        }
        Ok(_) => {}
        Err(e) => tracing::error!("DB read for {}: {}", doc_id, e),
    }

    // 2) Send back our own state vector so the client can push the
    //    inverse diff (whatever we are missing). Without this the Yrs
    //    sync protocol is one-way: a client whose subdoc had content
    //    we never observed (e.g. content authored before the server
    //    had a chance to record it, then a connection reset wiped the
    //    in-flight updates) would never re-broadcast that content,
    //    leaving it stranded on the client side forever.
    match state.db.get_doc_state_vector(doc_id).await {
        Ok(server_sv) => {
            let frame = encode_message(MSG_SYNC_STEP_1, doc_id, &server_sv);
            let _ = tx_out.send(frame);
        }
        Err(e) => tracing::error!("compute server SV for {}: {}", doc_id, e),
    }
}

/// True iff the encoded Yrs update has no blocks and an empty delete set.
/// Broadcasting an empty update is wasted bandwidth, and worse: it
/// triggers `flush_content_to_disk` on every other CLI client, which
/// can race with a concurrent local-disk delete (the freshly-deleted
/// file gets re-written from CRDT content before scan_once notices the
/// unlink, defeating delete propagation entirely).
fn is_noop_update(payload: &[u8]) -> bool {
    use yrs::Update;
    use yrs::updates::decoder::Decode;
    match Update::decode_v1(payload) {
        Ok(u) => u.state_vector().is_empty(),
        // Malformed: not no-op, but also can't apply — let downstream reject.
        Err(_) => false,
    }
}

async fn handle_content_update(
    state: &AppState,
    conn: uuid::Uuid,
    doc_id: &str,
    payload: &[u8],
) {
    // Skip empty STEP_2 replies (typically those that come back from a
    // client whose state vector matched ours after the handshake — the
    // client has nothing to add). Without this the bidirectional handshake
    // produces a thundering-herd of empty broadcasts, each one re-flushing
    // the content to disk on every peer and shadow-resurrecting freshly
    // deleted files before scan_once can register them as gone.
    if is_noop_update(payload) {
        return;
    }
    if let Err(e) = state.db.save_update(doc_id, payload).await {
        tracing::error!("persist content update for {}: {}", doc_id, e);
        return;
    }
    // Broadcast as MSG_UPDATE so late-arriving peers don't misread
    // a STEP_2 (which by convention is peer-directed, not broadcast).
    let frame = encode_message(MSG_UPDATE, doc_id, payload);
    let mut channels = state.channels.write().await;
    let tx = channels
        .entry(doc_id.to_string())
        .or_insert_with(|| broadcast::channel(PER_DOC_BROADCAST_CAP).0);
    let _ = tx.send((frame, conn));
}

async fn handle_blob_update(
    state: &AppState,
    conn: uuid::Uuid,
    doc_id: &str,
    payload: &[u8],
) {
    use sha2::{Digest, Sha256};
    if payload.len() > crate::protocol::MAX_BLOB_SIZE {
        tracing::warn!(
            "Rejected blob for {} — {} bytes exceeds {} byte limit",
            doc_id,
            payload.len(),
            crate::protocol::MAX_BLOB_SIZE
        );
        return;
    }
    let hash = format!("{:x}", Sha256::digest(payload));
    if let Err(e) = state.db.save_blob(&hash, payload).await {
        tracing::error!("save blob: {}", e);
        return;
    }
    let frame = encode_message(MSG_BLOB_UPDATE, doc_id, payload);
    if let Some(tx) = state.channels.read().await.get(doc_id) {
        let _ = tx.send((frame, conn));
    }
}

async fn handle_blob_request(
    state: &AppState,
    tx_out: &mpsc::UnboundedSender<Vec<u8>>,
    doc_id: &str,
    payload: &[u8],
) {
    let hash = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => return,
    };
    match state.db.load_blob(hash).await {
        Ok(Some(data)) => {
            let frame = encode_message(MSG_BLOB_UPDATE, doc_id, &data);
            let _ = tx_out.send(frame);
        }
        Ok(None) => {
            tracing::warn!("blob not found for {}: {}", doc_id, hash);
        }
        Err(e) => tracing::error!("load blob: {}", e),
    }
}

// ---------------------------------------------------------------------------
// Shared: ensure the current connection is subscribed to `doc_id`'s
// broadcast channel. Creates the channel on first use.
// ---------------------------------------------------------------------------

async fn ensure_subscribed(
    state: &AppState,
    doc_id: String,
    conn: uuid::Uuid,
    tx_out: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let rx = {
        let mut channels = state.channels.write().await;
        let tx = channels.entry(doc_id.clone()).or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(PER_DOC_BROADCAST_CAP);
            tx
        });
        tx.subscribe()
    };

    // Forwarding task: reads from the broadcast channel and pushes into
    // this connection's outgoing mpsc. Skips frames whose sender_id ==
    // this connection (don't echo our own writes). Exits when tx_out
    // is closed (connection gone).
    let tx_fwd = tx_out.clone();
    let db = state.db.clone();
    tokio::spawn(async move {
        let mut rx = rx;
        loop {
            tokio::select! {
                _ = tx_fwd.closed() => break,
                res = rx.recv() => match res {
                    Ok((framed, sender_id)) => {
                        if sender_id == conn {
                            continue;
                        }
                        if tx_fwd.send(framed).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Force a full resync from DB — idempotent for
                        // CRDT updates.
                        tracing::warn!(
                            "broadcast lagged by {} for {}: doing full catch-up",
                            n, doc_id
                        );
                        let is_manifest = doc_id == MANIFEST_DOC_ID;
                        match db.get_all_updates_since(&doc_id, &StateVector::default()).await {
                            Ok(update) if !update.is_empty() => {
                                let frame = if is_manifest {
                                    let mut p = Vec::with_capacity(1 + update.len());
                                    p.push(crate::protocol::MANIFEST_UPDATE);
                                    p.extend_from_slice(&update);
                                    encode_message(MSG_MANIFEST_SYNC, &doc_id, &p)
                                } else {
                                    encode_message(MSG_SYNC_STEP_2, &doc_id, &update)
                                };
                                if tx_fwd.send(frame).is_err() {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests — v1-specific. v0-era handshake + index-skew tests were removed
// when the wire protocol flipped; the equivalent v1 coverage lives in
// `tests/v1_server_e2e.rs`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::ids::ActorId;
    use crate::v1::sync::{encode_manifest_update, manifest_step1_payload};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
    use yrs::updates::encoder::Encode;
    use yrs::{ReadTxn, Transact};

    async fn setup_test_server() -> (u16, AppState) {
        let db = Db::new("sqlite::memory:").await.unwrap();
        let _ = migrate_server_db(&db).await.unwrap();
        let state = AppState {
            db,
            channels: Arc::new(RwLock::new(HashMap::new())),
            manifest: Arc::new(AsyncMutex::new(Manifest::new(ActorId::new()))),
            started_at: std::time::Instant::now(),
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

    async fn send_bin(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        data: Vec<u8>,
    ) {
        ws.send(TungsteniteMessage::Binary(data.into()))
            .await
            .unwrap();
    }

    async fn recv_bin(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Vec<u8> {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(TungsteniteMessage::Binary(b)))) => b.to_vec(),
            other => panic!("expected binary frame, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handshake_accepts_v1_and_echoes_version() {
        let (port, _) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws, _) = connect_async(url).await.unwrap();

        let frame = encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        );
        send_bin(&mut ws, frame).await;

        let echo = recv_bin(&mut ws).await;
        let (t, d, p) = decode_message(&echo).unwrap();
        assert_eq!(t, MSG_VERSION);
        assert_eq!(d, MANIFEST_DOC_ID);
        let (major, minor) = decode_version_handshake(p).unwrap();
        assert_eq!((major, minor), (V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR));
    }

    #[tokio::test]
    async fn handshake_rejects_v0_client_by_closing() {
        let (port, _) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws, _) = connect_async(url).await.unwrap();

        // v0 would send MSG_SYNC_STEP_1 immediately. The v1 server
        // closes the connection instead of replying.
        let sv = StateVector::default();
        use yrs::updates::encoder::Encode;
        let frame = encode_message(MSG_SYNC_STEP_1, "some.md", &sv.encode_v1());
        send_bin(&mut ws, frame).await;

        // Either the next recv yields Close/None or the stream drops.
        let outcome =
            tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
        match outcome {
            Ok(None) => {}
            Ok(Some(Ok(TungsteniteMessage::Close(_)))) => {}
            Ok(Some(Err(_))) => {} // abrupt close
            Ok(Some(Ok(other))) => panic!("unexpected frame: {:?}", other),
            Err(_) => panic!("server kept the socket open for a v0 client"),
        }
    }

    #[tokio::test]
    async fn manifest_step1_returns_step2_for_empty_client() {
        let (port, _) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws, _) = connect_async(url).await.unwrap();

        // Version handshake.
        send_bin(
            &mut ws,
            encode_message(
                MSG_VERSION,
                MANIFEST_DOC_ID,
                &encode_version_handshake(),
            ),
        )
        .await;
        let _ = recv_bin(&mut ws).await; // echo

        // Client's STEP_1 from a fresh (empty) manifest.
        let client_manifest = Manifest::new(ActorId::new());
        let p = manifest_step1_payload(&client_manifest);
        send_bin(
            &mut ws,
            encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &p),
        )
        .await;

        // Server responds with STEP_2 (empty update is fine, just the
        // sub-type byte + yrs state-as-update for a default SV).
        let resp = recv_bin(&mut ws).await;
        let (t, d, payload) = decode_message(&resp).unwrap();
        assert_eq!(t, MSG_MANIFEST_SYNC);
        assert_eq!(d, MANIFEST_DOC_ID);
        let (sub, _) = split_manifest_payload(payload).unwrap();
        assert_eq!(sub, crate::protocol::MANIFEST_STEP_2);
    }

    #[tokio::test]
    async fn manifest_update_persists_and_broadcasts() {
        let (port, state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);

        // Two clients both finish the version handshake.
        let (mut a, _) = connect_async(&url).await.unwrap();
        let (mut b, _) = connect_async(&url).await.unwrap();
        let hs = encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        );
        send_bin(&mut a, hs.clone()).await;
        send_bin(&mut b, hs).await;
        let _ = recv_bin(&mut a).await;
        let _ = recv_bin(&mut b).await;

        // Both subscribe via STEP_1 so they're registered in the
        // manifest broadcast channel. The server replies with STEP_2
        // *and* its own STEP_1 (bidirectional handshake), both of which
        // need draining before we can observe broadcasts.
        let empty_p =
            manifest_step1_payload(&Manifest::new(ActorId::new()));
        let sub_frame =
            encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &empty_p);
        send_bin(&mut a, sub_frame.clone()).await;
        let _ = recv_bin(&mut a).await;
        let _ = recv_bin(&mut a).await;
        send_bin(&mut b, sub_frame).await;
        let _ = recv_bin(&mut b).await;
        let _ = recv_bin(&mut b).await;

        // A creates a node; encodes its update as MANIFEST_UPDATE and
        // sends it. B should receive the same bytes (minus A's sub-
        // type remapping).
        use crate::v1::ops::create_text;
        let mut author = Manifest::new(ActorId::new());
        let _id = create_text(&mut author, "hello.md", 5).unwrap();
        let update_bytes = author
            .doc()
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let p = encode_manifest_update(&update_bytes);
        send_bin(
            &mut a,
            encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &p),
        )
        .await;

        // B receives the broadcast.
        let echoed = recv_bin(&mut b).await;
        let (t, d, payload) = decode_message(&echoed).unwrap();
        assert_eq!(t, MSG_MANIFEST_SYNC);
        assert_eq!(d, MANIFEST_DOC_ID);
        let (sub, _) = split_manifest_payload(payload).unwrap();
        assert_eq!(sub, crate::protocol::MANIFEST_UPDATE);

        // Server persisted the inner yrs update under __manifest__.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stored = state.db.load_doc_updates(MANIFEST_DOC_ID).await.unwrap();
        assert!(
            !stored.is_empty(),
            "server should have persisted the manifest update"
        );
    }

    #[tokio::test]
    async fn v1_blob_upload_then_request_roundtrip() {
        // Pins the 3.3c contract: a v1 client uploads a blob addressed
        // by its own hex hash, and a later MSG_BLOB_REQUEST for that hash
        // returns the original bytes.
        let (port, state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws, _) = connect_async(url).await.unwrap();

        // Handshake.
        send_bin(
            &mut ws,
            encode_message(
                MSG_VERSION,
                MANIFEST_DOC_ID,
                &encode_version_handshake(),
            ),
        )
        .await;
        let _ = recv_bin(&mut ws).await;

        // Upload: doc_id = hash_hex, payload = bytes. The server derives
        // the storage key from the payload itself, so whatever doc_id we
        // use is purely for routing — v1 clients use the hash for both.
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\nhello binary";
        use sha2::{Digest, Sha256};
        let hash_hex = format!("{:x}", Sha256::digest(bytes));
        send_bin(
            &mut ws,
            encode_message(MSG_BLOB_UPDATE, &hash_hex, bytes),
        )
        .await;

        // Give the server a beat to persist before we query.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stored = state.db.load_blob(&hash_hex).await.unwrap();
        assert_eq!(stored.as_deref(), Some(bytes));

        // Request: payload = hash_hex as utf8 (server's current contract).
        send_bin(
            &mut ws,
            encode_message(MSG_BLOB_REQUEST, &hash_hex, hash_hex.as_bytes()),
        )
        .await;
        let resp = recv_bin(&mut ws).await;
        let (t, d, payload) = decode_message(&resp).unwrap();
        assert_eq!(t, MSG_BLOB_UPDATE);
        assert_eq!(d, hash_hex);
        assert_eq!(payload, bytes);
    }

    /// Pins the bidirectional content-sync handshake. When a client
    /// sends `MSG_SYNC_STEP_1` for a content subdoc the server has
    /// nothing for, the server must still reciprocate with its own
    /// `MSG_SYNC_STEP_1` (carrying its state vector — empty or not)
    /// so the client knows to push its updates back. Without this,
    /// content authored by the client and never re-edited never
    /// reaches the server.
    #[tokio::test]
    async fn server_reciprocates_step1_for_empty_content_doc() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws, _) = connect_async(url).await.unwrap();

        // Handshake.
        send_bin(
            &mut ws,
            encode_message(
                MSG_VERSION,
                MANIFEST_DOC_ID,
                &encode_version_handshake(),
            ),
        )
        .await;
        let _ = recv_bin(&mut ws).await;

        // Client subscribes to a content doc the server has never seen,
        // sending an empty state vector.
        let doc_id = "content:019dc69a-1234-7000-8000-000000000001";
        let empty_sv = yrs::StateVector::default().encode_v1();
        send_bin(
            &mut ws,
            encode_message(MSG_SYNC_STEP_1, doc_id, &empty_sv),
        )
        .await;

        // Expect a STEP_1 back from the server (its SV) so we know it
        // wants whatever we have. The server may also send STEP_2 first
        // (with empty/non-empty content) — accept either ordering.
        let mut saw_step1 = false;
        for _ in 0..2 {
            let resp = recv_bin(&mut ws).await;
            let (t, d, _payload) = decode_message(&resp).unwrap();
            assert_eq!(d, doc_id);
            if t == MSG_SYNC_STEP_1 {
                saw_step1 = true;
                break;
            }
            assert_eq!(t, MSG_SYNC_STEP_2, "unexpected msg type {:#x}", t);
        }
        assert!(
            saw_step1,
            "server must reciprocate STEP_1 even when it has no content for the doc"
        );
    }

    // =====================================================================
    // auto-apr28-006: adversarial — feed the server a barrage of
    // garbage frames over a real WS, then prove a follow-up well-formed
    // client can still complete a clean handshake on a NEW connection.
    // The server must not crash, leak the connection's resources, or
    // poison subsequent clients.
    // =====================================================================
    #[tokio::test]
    async fn auto_apr28_006_server_survives_garbage_frames() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);

        // Connection 1: open, blast garbage, drop.
        let (mut ws_bad, _) = connect_async(&url).await.unwrap();

        // Frame 1: too short to even hold a msg_type byte.
        ws_bad
            .send(TungsteniteMessage::Binary(vec![].into()))
            .await
            .unwrap();
        ws_bad
            .send(TungsteniteMessage::Binary(vec![0x42].into()))
            .await
            .unwrap();

        // Frame 2: msg_type = 0xFF (unknown), arbitrary tail.
        ws_bad
            .send(TungsteniteMessage::Binary(
                vec![0xff, 0xde, 0xad, 0xbe, 0xef].into(),
            ))
            .await
            .unwrap();

        // Frame 3: well-framed MSG_UPDATE on a manifest doc with
        // garbage payload bytes that aren't a valid Yrs update.
        let bad_update = encode_message(MSG_UPDATE, MANIFEST_DOC_ID, &[0xfa, 0xce, 0xfe, 0xed]);
        ws_bad
            .send(TungsteniteMessage::Binary(bad_update.into()))
            .await
            .unwrap();

        // Frame 4: well-framed MSG_SYNC_STEP_1 with a state-vector
        // payload that's truncated.
        let bad_sv = encode_message(
            MSG_SYNC_STEP_1,
            "content:00000000-0000-0000-0000-000000000000",
            &[0xff, 0xff, 0xff],
        );
        ws_bad
            .send(TungsteniteMessage::Binary(bad_sv.into()))
            .await
            .unwrap();

        // Frame 5: a giant payload (1 MiB) of zeros claiming to be a
        // MSG_BLOB_UPDATE with a bogus 'hash' as doc_id.
        let big_garbage = vec![0u8; 1024 * 1024];
        let big_frame = encode_message(MSG_BLOB_UPDATE, "deadbeef", &big_garbage);
        ws_bad
            .send(TungsteniteMessage::Binary(big_frame.into()))
            .await
            .unwrap();

        // Drop the bad connection without a clean close. Give the
        // server a moment to clean up.
        drop(ws_bad);
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Connection 2: a fresh, well-behaved client must still
        // complete a clean v1 handshake. If the server is wedged or
        // crashed, this connect_async will fail or the handshake will
        // time out.
        let (mut ws_good, _) = connect_async(&url).await.unwrap();
        let frame = encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        );
        send_bin(&mut ws_good, frame).await;
        let echo = recv_bin(&mut ws_good).await;
        let (t, d, p) = decode_message(&echo).expect("server echoed framed VERSION");
        assert_eq!(t, MSG_VERSION);
        assert_eq!(d, MANIFEST_DOC_ID);
        let (major, minor) = decode_version_handshake(p).expect("valid version payload");
        assert_eq!((major, minor), (V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR));

        // And a normal manifest STEP_1 should still get a STEP_2.
        let manifest = Manifest::new(ActorId::new());
        let step1 = manifest_step1_payload(&manifest);
        let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &step1);
        send_bin(&mut ws_good, frame).await;
        // We expect a STEP_2 reply (server's own) and possibly a
        // STEP_1 (server soliciting our state). Just verify *something*
        // framed comes back within 2s.
        let reply = recv_bin(&mut ws_good).await;
        let (t, d, _p) = decode_message(&reply).expect("server reply is well-framed");
        assert_eq!(d, MANIFEST_DOC_ID);
        assert!(
            t == MSG_MANIFEST_SYNC,
            "expected MSG_MANIFEST_SYNC reply, got msg_type 0x{:02x}",
            t
        );
    }

    // =====================================================================
    // auto-apr28-022: MSG_SERVER_STATS request/response shape stability.
    // The JSON keys must be present and typed correctly so existing
    // clients don't break across server upgrades.
    // =====================================================================
    #[tokio::test]
    async fn auto_apr28_022_server_stats_returns_well_formed_json() {
        let (port, _state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws, _) = connect_async(url).await.unwrap();

        // Handshake first so the connection is established.
        let frame = encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        );
        send_bin(&mut ws, frame).await;
        let _ = recv_bin(&mut ws).await;

        // Request stats. Empty payload.
        let stats_req = encode_message(MSG_SERVER_STATS, MANIFEST_DOC_ID, &[]);
        send_bin(&mut ws, stats_req).await;

        let reply = recv_bin(&mut ws).await;
        let (t, d, p) = decode_message(&reply).expect("stats reply well-framed");
        assert_eq!(t, MSG_SERVER_STATS);
        assert_eq!(d, MANIFEST_DOC_ID);

        let json: serde_json::Value =
            serde_json::from_slice(p).expect("stats payload is valid JSON");

        // Required keys. Drift on any of these breaks every client
        // that consumes the stats endpoint.
        for key in &[
            "server_version",
            "uptime_secs",
            "connected_clients",
            "manifest_node_count",
            "manifest_total_bytes",
            "blob_count",
            "blob_total_bytes",
        ] {
            assert!(
                json.get(*key).is_some(),
                "stats JSON missing key {key}; got: {json}"
            );
        }

        // Type sanity: numerics are numbers, version is a string.
        assert!(json["server_version"].is_string());
        assert!(json["uptime_secs"].is_u64());
        assert!(json["connected_clients"].is_u64());
        assert!(json["manifest_node_count"].is_u64());
        assert!(json["manifest_total_bytes"].is_u64());
        assert!(json["blob_count"].is_u64());
        assert!(json["blob_total_bytes"].is_u64());

        // connected_clients should be ≥ 1 (us, currently subscribed
        // — actually the manifest channel is created on first
        // MANIFEST_SYNC subscribe. Stats request alone might not
        // subscribe. So accept ≥ 0.
        assert!(json["connected_clients"].as_u64().unwrap() <= 1);
    }

    // =====================================================================
    // auto-apr28-026: a malicious peer sends a MSG_BLOB_UPDATE with a
    // doc_id (the "claimed hash") that does NOT match the SHA-256 of
    // the payload. The server must:
    //   - re-compute the actual SHA-256 of the payload
    //   - persist the blob under the *true* hash (not the claimed one)
    // After the bad peer disconnects, an honest client requesting
    // the TRUE hash must get the correct bytes; requesting the
    // claimed (fake) hash must fail.
    // =====================================================================
    #[tokio::test]
    async fn auto_apr28_026_blob_update_with_mismatched_doc_id_stores_under_true_hash() {
        use sha2::{Digest, Sha256};
        let (port, state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);
        let (mut ws, _) = connect_async(url).await.unwrap();

        // Handshake.
        let frame = encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        );
        send_bin(&mut ws, frame).await;
        let _ = recv_bin(&mut ws).await;

        // Send a blob with mismatched doc_id.
        let real_payload: &[u8] = b"the real content";
        let true_hash = format!("{:x}", Sha256::digest(real_payload));
        let claimed_fake_hash = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        assert_ne!(claimed_fake_hash, true_hash);

        let bad_frame = encode_message(MSG_BLOB_UPDATE, claimed_fake_hash, real_payload);
        send_bin(&mut ws, bad_frame).await;

        // Give the server a moment to persist.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // The blob is stored under the TRUE hash.
        let loaded_true = state
            .db
            .load_blob(&true_hash)
            .await
            .expect("load blob with true hash");
        assert_eq!(
            loaded_true.as_deref(),
            Some(real_payload),
            "blob must be stored under the actual SHA-256, not the claimed one"
        );

        // The claimed (fake) hash is NOT a key in the blob table.
        let loaded_fake = state
            .db
            .load_blob(claimed_fake_hash)
            .await
            .expect("load blob with claimed fake hash");
        assert!(
            loaded_fake.is_none(),
            "no blob should be stored under the (forged) claimed hash"
        );
    }

    // =====================================================================
    // auto-apr28-029: MSG_MANIFEST_VERIFY behaviour. When a client
    // claims a projection hash that DOESN'T match the server's, the
    // server must respond with a MSG_MANIFEST_SYNC STEP_1 (kicking
    // off a re-sync). When the hashes match, server stays silent
    // (converged). Reactive: the test verifies both branches.
    // =====================================================================
    #[tokio::test]
    async fn auto_apr28_029_verify_with_mismatch_triggers_step1_else_silent() {
        let (port, state) = setup_test_server().await;
        let url = format!("ws://127.0.0.1:{}/sync", port);

        // Populate the server with one manifest entry so the verify
        // hash is non-trivial.
        {
            let mut m = state.manifest.lock().await;
            crate::v1::create_text(&mut m, "anchor.md", 0).unwrap();
        }

        // CASE A: send VERIFY with a wrong (all-zero) hash → expect
        // a MSG_MANIFEST_SYNC reply (STEP_1).
        let (mut ws_a, _) = connect_async(&url).await.unwrap();
        let frame_v = encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        );
        send_bin(&mut ws_a, frame_v).await;
        let _ = recv_bin(&mut ws_a).await; // VERSION echo

        let bogus_hash = [0u8; 32];
        let verify_payload =
            crate::v1::sync::encode_verify_payload(&bogus_hash);
        let verify_frame =
            encode_message(MSG_MANIFEST_VERIFY, MANIFEST_DOC_ID, &verify_payload);
        send_bin(&mut ws_a, verify_frame).await;

        let reply = recv_bin(&mut ws_a).await;
        let (t, d, _p) = decode_message(&reply).expect("verify reply framed");
        assert_eq!(t, MSG_MANIFEST_SYNC, "mismatch should trigger MANIFEST_SYNC");
        assert_eq!(d, MANIFEST_DOC_ID);
        drop(ws_a);

        // CASE B: send VERIFY with the CORRECT hash → expect silence.
        let true_hash = {
            let manifest = state.manifest.lock().await;
            crate::v1::projection_hash(&manifest)
        };
        let (mut ws_b, _) = connect_async(&url).await.unwrap();
        let frame_v = encode_message(
            MSG_VERSION,
            MANIFEST_DOC_ID,
            &encode_version_handshake(),
        );
        send_bin(&mut ws_b, frame_v).await;
        let _ = recv_bin(&mut ws_b).await;

        let verify_payload = crate::v1::sync::encode_verify_payload(&true_hash);
        let verify_frame =
            encode_message(MSG_MANIFEST_VERIFY, MANIFEST_DOC_ID, &verify_payload);
        send_bin(&mut ws_b, verify_frame).await;

        // Wait briefly — if server is going to respond, it'd be
        // within ~50ms. Use a short timeout: silence == converged.
        let recv_result = tokio::time::timeout(Duration::from_millis(500), ws_b.next()).await;
        assert!(
            recv_result.is_err(),
            "match should produce silence; got reply: {:?}",
            recv_result
        );
    }
}
