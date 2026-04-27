//! WASM bindings for the v1 manifest-CRDT protocol.
//!
//! Mirrors the shape of [`crate::wasm_client::SynclineClient`] but speaks
//! v1 on the wire: a `MSG_VERSION` handshake, a single manifest Y.Doc
//! under [`MANIFEST_DOC_ID`] driven by `MSG_MANIFEST_SYNC`, and per-node
//! content subdocs addressed as `content:<nodeid-hyphenated>`.
//!
//! Persistence and filesystem I/O live in TypeScript — this binding only
//! owns the in-memory CRDT state and the WebSocket. Persistence hooks
//! fire after every successful sync so the plugin can snapshot Yrs
//! updates to disk and replay them on restart.
//!
//! Shared state is held in per-field `Rc<RefCell<…>>`s so WebSocket
//! event closures and CRDT observers can mutate only what they need
//! without colliding on a monolithic borrow — this is the same pattern
//! as `wasm_client.rs`.

use js_sys::{Date, Function, Uint8Array};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Subscription, Text, Transact, Update};

use crate::protocol::{
    decode_message, encode_message, MANIFEST_DOC_ID, MSG_BLOB_REQUEST, MSG_BLOB_UPDATE,
    MSG_MANIFEST_SYNC, MSG_MANIFEST_VERIFY, MSG_SERVER_STATS, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2,
    MSG_UPDATE, MSG_VERSION,
};
use crate::v1::hash::hash_hex;
use crate::v1::ids::{ActorId, Lamport, NodeId};
use crate::v1::manifest::Manifest;
use crate::v1::ops;
use crate::v1::projection::{project, ProjectedEntry};
use crate::v1::sync::{
    decode_verify_payload, decode_version_handshake, encode_manifest_update, encode_verify_payload,
    encode_version_handshake, handle_manifest_payload, manifest_step1_payload, projection_hash,
};

// ---------------------------------------------------------------------------
// Per-subdoc state
// ---------------------------------------------------------------------------

/// Tracked content subdoc. Kept alive by `_sub`; `is_receiving` blocks
/// the observer from re-broadcasting updates we just applied from the
/// wire.
struct ContentDoc {
    doc: Doc,
    _sub: Subscription,
    is_receiving: Rc<RefCell<bool>>,
}

// ---------------------------------------------------------------------------
// Public binding
// ---------------------------------------------------------------------------

/// Low-level v1 protocol client. One instance per vault.
///
/// Lifecycle:
///   1. `new(url, actorIdHex?)`.
///   2. Either `initManifest()` for a fresh vault, or
///      `loadManifestState(bytes, lamport)` to resume.
///   3. Register callbacks (`onManifestChanged`, `onContentChanged`,
///      `onBlob`, `onStatus`).
///   4. `connect()`.
///
/// The client awaits the server's `MSG_VERSION` before sending any
/// manifest sync frames.
#[wasm_bindgen]
pub struct SynclineV1Client {
    url: String,
    actor: ActorId,
    manifest: Rc<RefCell<Option<Manifest>>>,
    _manifest_sub: Rc<RefCell<Option<Subscription>>>,
    manifest_is_receiving: Rc<RefCell<bool>>,
    content: Rc<RefCell<HashMap<NodeId, ContentDoc>>>,
    ws: Rc<RefCell<Option<WebSocket>>>,
    is_connected: Rc<RefCell<bool>>,
    closures: Rc<RefCell<Vec<Closure<dyn FnMut(JsValue)>>>>,
    requested_blobs: Rc<RefCell<HashMap<String, f64>>>,
    /// Content node ids whose STEP_1 was deferred because the WebSocket
    /// hadn't completed its handshake yet. Drained on `onopen`. Without
    /// this, `subscribeContent` calls during the connect() → onopen
    /// gap would silently no-op and the matching node would never see
    /// remote updates (#57).
    pending_step1: Rc<RefCell<HashSet<NodeId>>>,
    /// Outcome of the most recent [`send_verify`] round-trip — used by
    /// the sidebar's `degraded` indicator (#65 §6). Lifecycle:
    ///
    ///   `Unknown` (initial) → `Pending` (we just sent VERIFY) →
    ///   `Match` / `Mismatch` (server replied or we observed the
    ///    inferred outcome via the SyncStep1 fix-up path).
    ///
    /// `&'static str` keeps the WASM boundary trivial — no enum
    /// marshalling — and matches what the JS side expects.
    last_verify_result: Rc<RefCell<&'static str>>,
    on_manifest_changed: Rc<RefCell<Option<Function>>>,
    on_content_changed: Rc<RefCell<Option<Function>>>,
    on_blob: Rc<RefCell<Option<Function>>>,
    on_status: Rc<RefCell<Option<Function>>>,
    /// Fires whenever `requested_blobs` grows or shrinks. Argument is
    /// `(pendingCount: number, pendingBytes: number)`.
    on_blob_queue_change: Rc<RefCell<Option<Function>>>,
    /// Fires once per "user-visible sync event" — see
    /// [`emit_sync_event`] for the kinds. Argument is a JSON-stringified
    /// `{ kind, path?, hash?, bytes?, error? }`. The plugin keeps a
    /// small ring buffer of these for the activity feed.
    on_sync_event: Rc<RefCell<Option<Function>>>,
    /// Fires when a `MSG_SERVER_STATS` reply arrives. Argument is the
    /// JSON string the server sent. Plugin's sidebar `JSON.parse`s it
    /// into `{ server_version, uptime_secs, connected_clients, … }`.
    on_server_stats: Rc<RefCell<Option<Function>>>,
}

#[wasm_bindgen]
impl SynclineV1Client {
    /// Construct a fresh client. `actor_id_hex` is the persisted per-
    /// installation `ActorId` (UUID, hyphenated). Pass `None` or an
    /// empty string to mint a new one — the caller must persist what
    /// `actorId()` returns so the same identity is reused.
    #[wasm_bindgen(constructor)]
    pub fn new(url: String, actor_id_hex: Option<String>) -> Result<SynclineV1Client, JsValue> {
        console_error_panic_hook::set_once();
        let actor = match actor_id_hex.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => ActorId::parse_str(s)
                .ok_or_else(|| JsValue::from_str(&format!("invalid actor id: {s}")))?,
            None => ActorId::new(),
        };
        Ok(SynclineV1Client {
            url,
            actor,
            manifest: Rc::new(RefCell::new(None)),
            _manifest_sub: Rc::new(RefCell::new(None)),
            manifest_is_receiving: Rc::new(RefCell::new(false)),
            content: Rc::new(RefCell::new(HashMap::new())),
            ws: Rc::new(RefCell::new(None)),
            is_connected: Rc::new(RefCell::new(false)),
            closures: Rc::new(RefCell::new(Vec::new())),
            requested_blobs: Rc::new(RefCell::new(HashMap::new())),
            pending_step1: Rc::new(RefCell::new(HashSet::new())),
            last_verify_result: Rc::new(RefCell::new("unknown")),
            on_manifest_changed: Rc::new(RefCell::new(None)),
            on_content_changed: Rc::new(RefCell::new(None)),
            on_blob: Rc::new(RefCell::new(None)),
            on_status: Rc::new(RefCell::new(None)),
            on_blob_queue_change: Rc::new(RefCell::new(None)),
            on_sync_event: Rc::new(RefCell::new(None)),
            on_server_stats: Rc::new(RefCell::new(None)),
        })
    }

    #[wasm_bindgen(js_name = actorId)]
    pub fn actor_id(&self) -> String {
        self.actor.to_string_hyphenated()
    }

    // ---------------------------------------------------------------
    // Manifest lifecycle
    // ---------------------------------------------------------------

    #[wasm_bindgen(js_name = initManifest)]
    pub fn init_manifest(&self) -> Result<(), JsValue> {
        if self.manifest.borrow().is_some() {
            return Ok(());
        }
        let manifest = Manifest::new(self.actor);
        self.install_manifest_observer(&manifest);
        *self.manifest.borrow_mut() = Some(manifest);
        Ok(())
    }

    /// Rehydrate the manifest from a persisted Yrs update. `lamport`
    /// is the last-observed counter from `.syncline/lamport`.
    /// `lamport` is taken as `f64` so it can be passed as a plain JS Number
    /// (wasm-bindgen marshals `u64` as BigInt, which the plugin's persisted
    /// counter is not). Lamport stamps in practice stay well below 2^53.
    #[wasm_bindgen(js_name = loadManifestState)]
    pub fn load_manifest_state(&self, state: &[u8], lamport: f64) -> Result<(), JsValue> {
        let manifest = Manifest::from_update(self.actor, Lamport(lamport as u64), state)
            .map_err(|e| JsValue::from_str(&format!("load_manifest_state: {e}")))?;
        self.install_manifest_observer(&manifest);
        *self.manifest.borrow_mut() = Some(manifest);
        Ok(())
    }

    #[wasm_bindgen(js_name = manifestSnapshot)]
    pub fn manifest_snapshot(&self) -> Uint8Array {
        match self.manifest.borrow().as_ref() {
            Some(m) => Uint8Array::from(&m.encode_state_as_update()[..]),
            None => Uint8Array::new_with_length(0),
        }
    }

    /// Returned as `f64` so the JS side gets a plain Number — same reasoning
    /// as `loadManifestState`'s `lamport` arg.
    #[wasm_bindgen(js_name = lamport)]
    pub fn lamport_value(&self) -> f64 {
        self.manifest
            .borrow()
            .as_ref()
            .map(|m| m.lamport().get() as f64)
            .unwrap_or(0.0)
    }

    fn install_manifest_observer(&self, manifest: &Manifest) {
        let is_receiving = self.manifest_is_receiving.clone();
        let ws = self.ws.clone();
        let is_connected = self.is_connected.clone();
        let on_changed = self.on_manifest_changed.clone();

        let sub = manifest
            .doc()
            .observe_update_v1(move |_, event| {
                if *is_receiving.borrow() {
                    return;
                }
                if *is_connected.borrow() {
                    if let Some(ws) = ws.borrow().as_ref() {
                        let payload = encode_manifest_update(&event.update);
                        let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &payload);
                        send_frame(ws, &frame);
                    }
                }
                // Callback is snapshot-cloned to drop the borrow before calling into JS.
                let cb = on_changed.borrow().clone();
                if let Some(cb) = cb {
                    let _ = cb.call0(&JsValue::NULL);
                }
            })
            .ok();
        *self._manifest_sub.borrow_mut() = sub;
    }

    // ---------------------------------------------------------------
    // Callbacks
    // ---------------------------------------------------------------

    #[wasm_bindgen(js_name = onManifestChanged)]
    pub fn set_on_manifest_changed(&self, cb: Function) {
        *self.on_manifest_changed.borrow_mut() = Some(cb);
    }

    #[wasm_bindgen(js_name = onContentChanged)]
    pub fn set_on_content_changed(&self, cb: Function) {
        *self.on_content_changed.borrow_mut() = Some(cb);
    }

    #[wasm_bindgen(js_name = onBlob)]
    pub fn set_on_blob(&self, cb: Function) {
        *self.on_blob.borrow_mut() = Some(cb);
    }

    #[wasm_bindgen(js_name = onStatus)]
    pub fn set_on_status(&self, cb: Function) {
        *self.on_status.borrow_mut() = Some(cb);
    }

    /// Subscribe to blob-queue changes. Fires `(pendingCount,
    /// pendingBytes)` whenever the in-flight blob request set grows or
    /// shrinks. Drives the sidebar's "Downloading X / Y blobs" row
    /// (#65 §2). The plugin already has [`pending_blob_count`] /
    /// [`pending_blob_bytes`] for poll-style reads — this hook just
    /// avoids a 1 s timer on the JS side.
    #[wasm_bindgen(js_name = onBlobQueueChange)]
    pub fn set_on_blob_queue_change(&self, cb: Function) {
        *self.on_blob_queue_change.borrow_mut() = Some(cb);
    }

    /// Subscribe to discrete sync events for the activity feed (#65
    /// §5). Argument is a JSON string with shape
    /// `{ kind: "blob_down" | "blob_up" | "node_modified" | "error",
    ///    path?: string, hash?: string, bytes?: number, error?: string }`.
    /// Sent as a string (rather than a structured `JsValue`) so the
    /// plugin can `JSON.parse` and store directly into a UI ring
    /// buffer.
    #[wasm_bindgen(js_name = onSyncEvent)]
    pub fn set_on_sync_event(&self, cb: Function) {
        *self.on_sync_event.borrow_mut() = Some(cb);
    }

    /// Subscribe to server-stats replies (#65 §4). Argument is the
    /// raw JSON string. Plugin polls via [`request_server_stats`]
    /// (e.g. every 30 s) and updates the sidebar's "Server" section
    /// from each reply.
    #[wasm_bindgen(js_name = onServerStats)]
    pub fn set_on_server_stats(&self, cb: Function) {
        *self.on_server_stats.borrow_mut() = Some(cb);
    }

    /// Send an empty `MSG_SERVER_STATS` request; the reply arrives via
    /// the `onServerStats` callback. No-op if not connected — the
    /// caller can retry on the next timer tick.
    #[wasm_bindgen(js_name = requestServerStats)]
    pub fn request_server_stats(&self) -> Result<(), JsValue> {
        if !*self.is_connected.borrow() {
            return Err(JsValue::from_str("request_server_stats: not connected"));
        }
        let ws = self.ws.borrow();
        let ws = ws
            .as_ref()
            .ok_or_else(|| JsValue::from_str("request_server_stats: no socket"))?;
        let frame = encode_message(MSG_SERVER_STATS, MANIFEST_DOC_ID, &[]);
        send_frame(ws, &frame);
        Ok(())
    }

    // ---------------------------------------------------------------
    // Sidebar getters (#65 — pure reads, no protocol changes)
    // ---------------------------------------------------------------

    /// Number of blobs we've requested from the server but not yet
    /// received. Derived from the `requested_blobs` set.
    #[wasm_bindgen(js_name = pendingBlobCount)]
    pub fn pending_blob_count(&self) -> u32 {
        self.requested_blobs.borrow().len() as u32
    }

    /// Estimated number of bytes still to download — sum of
    /// `manifest.size` for every live binary entry whose blob hash is
    /// in the pending set. Uses `f64` rather than `u64` so the JS
    /// boundary marshals as a Number.
    #[wasm_bindgen(js_name = pendingBlobBytes)]
    pub fn pending_blob_bytes(&self) -> f64 {
        let manifest = self.manifest.borrow();
        let Some(m) = manifest.as_ref() else {
            return 0.0;
        };
        let pending = self.requested_blobs.borrow();
        if pending.is_empty() {
            return 0.0;
        }
        let mut total: u64 = 0;
        for entry in m.live_entries() {
            if entry.chunk_hashes.iter().any(|h| pending.contains_key(h)) {
                total = total.saturating_add(entry.size);
            }
        }
        total as f64
    }

    /// Number of content subdocs the client is actively subscribed
    /// to. Roughly tracks "open text files we'd see live updates for".
    #[wasm_bindgen(js_name = subscribedContentCount)]
    pub fn subscribed_content_count(&self) -> u32 {
        self.content.borrow().len() as u32
    }

    /// Number of live (non-deleted) entries in the manifest projection.
    /// What the user sees as "files in this vault" before conflict
    /// suffixes peel off.
    #[wasm_bindgen(js_name = manifestNodeCount)]
    pub fn manifest_node_count(&self) -> u32 {
        match self.manifest.borrow().as_ref() {
            Some(m) => m.live_entries().len() as u32,
            None => 0,
        }
    }

    /// Outcome of the most recent VERIFY round-trip:
    /// `"unknown" | "pending" | "match" | "mismatch"`. Drives the
    /// sidebar's `degraded` indicator (#65 §6).
    #[wasm_bindgen(js_name = lastVerifyResult)]
    pub fn last_verify_result_js(&self) -> String {
        (*self.last_verify_result.borrow()).to_string()
    }

    // ---------------------------------------------------------------
    // Connection
    // ---------------------------------------------------------------

    pub fn connect(&self) -> Result<(), JsValue> {
        if self.manifest.borrow().is_none() {
            return Err(JsValue::from_str(
                "connect: manifest not initialised — call initManifest or loadManifestState first",
            ));
        }
        if self.ws.borrow().is_some() {
            return Err(JsValue::from_str("connect: already connected"));
        }

        let ws = WebSocket::new(&self.url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        // ON OPEN
        let ws_open = ws.clone();
        let is_connected_open = self.is_connected.clone();
        let on_status_open = self.on_status.clone();
        let pending_step1_open = self.pending_step1.clone();
        let content_open = self.content.clone();
        let onopen = Closure::wrap(Box::new(move |_| {
            let payload = encode_version_handshake();
            let frame = encode_message(MSG_VERSION, MANIFEST_DOC_ID, &payload);
            send_frame(&ws_open, &frame);
            *is_connected_open.borrow_mut() = true;
            fire_status(&on_status_open, "connected");

            // Flush any STEP_1s that were queued while we were
            // disconnected (#57). Each entry corresponds to a content
            // subdoc the caller asked us to subscribe to before the
            // WebSocket finished its handshake — without this drain
            // their first sync round never starts.
            let to_send: Vec<NodeId> =
                pending_step1_open.borrow_mut().drain().collect();
            for node_id in to_send {
                let sv_bytes = match content_open.borrow().get(&node_id) {
                    Some(cd) => cd.doc.transact().state_vector().encode_v1(),
                    None => continue,
                };
                let frame = encode_message(
                    MSG_SYNC_STEP_1,
                    &content_doc_id(node_id),
                    &sv_bytes,
                );
                send_frame(&ws_open, &frame);
            }
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onopen);

        // ON MESSAGE
        let ws_msg = ws.clone();
        let handles = self.handles();
        let onmessage = Closure::wrap(Box::new(move |val: JsValue| {
            let Ok(e) = val.dyn_into::<MessageEvent>() else {
                return;
            };
            let Ok(ab) = e.data().dyn_into::<js_sys::ArrayBuffer>() else {
                return;
            };
            let data = Uint8Array::new(&ab).to_vec();
            dispatch_frame(&handles, &ws_msg, &data);
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onmessage);

        // ON ERROR
        let on_status_err = self.on_status.clone();
        let onerror = Closure::wrap(Box::new(move |val: JsValue| {
            web_sys::console::error_2(&JsValue::from_str("[SynclineV1] ws error"), &val);
            fire_status(&on_status_err, "error");
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onerror);

        // ON CLOSE
        let is_connected_close = self.is_connected.clone();
        let on_status_close = self.on_status.clone();
        let onclose = Closure::wrap(Box::new(move |_| {
            *is_connected_close.borrow_mut() = false;
            fire_status(&on_status_close, "disconnected");
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onclose);

        *self.ws.borrow_mut() = Some(ws);
        Ok(())
    }

    pub fn disconnect(&self) {
        if let Some(ws) = self.ws.borrow_mut().take() {
            ws.set_onopen(None);
            ws.set_onmessage(None);
            ws.set_onerror(None);
            ws.set_onclose(None);
            let _ = ws.close();
        }
        self.closures.borrow_mut().clear();
        *self.is_connected.borrow_mut() = false;
    }

    #[wasm_bindgen(js_name = isConnected)]
    pub fn is_connected(&self) -> bool {
        *self.is_connected.borrow()
    }

    // ---------------------------------------------------------------
    // Manifest ops (path-level)
    // ---------------------------------------------------------------

    // Size args are exposed as `f64` for the same reason as `lamport` —
    // wasm-bindgen renders `u64` as JS BigInt, but the plugin always has
    // these as plain Numbers (TextEncoder.encode().byteLength, file.stat.size).
    #[wasm_bindgen(js_name = createText)]
    pub fn create_text(&self, path: String, size: f64) -> Result<String, JsValue> {
        with_manifest_mut(&self.manifest, |m| {
            ops::create_text(m, &path, size as u64)
                .map(|id| id.to_string_hyphenated())
                .map_err(to_js)
        })
    }

    #[wasm_bindgen(js_name = createTextAllowingCollision)]
    pub fn create_text_allowing_collision(
        &self,
        path: String,
        size: f64,
    ) -> Result<String, JsValue> {
        with_manifest_mut(&self.manifest, |m| {
            ops::create_text_allowing_collision(m, &path, size as u64)
                .map(|id| id.to_string_hyphenated())
                .map_err(to_js)
        })
    }

    /// Create a binary entry from a **single** blob hash. Convenience
    /// wrapper around the chunk-aware API for callers that don't (yet)
    /// chunk their inputs — the hash becomes the sole entry in the new
    /// node's `chunk_hashes` list. New callers should prefer
    /// [`create_binary_chunked`] so files > 4 MiB don't blow the WS
    /// frame ceiling.
    #[wasm_bindgen(js_name = createBinary)]
    pub fn create_binary(
        &self,
        path: String,
        blob_hash_hex: String,
        size: f64,
    ) -> Result<String, JsValue> {
        let chunk_hashes = vec![blob_hash_hex];
        with_manifest_mut(&self.manifest, |m| {
            ops::create_binary(m, &path, &chunk_hashes, size as u64)
                .map(|id| id.to_string_hyphenated())
                .map_err(to_js)
        })
    }

    /// Create a binary entry from an explicit list of chunk hashes.
    /// `chunk_hashes_csv` is a comma-separated list of lowercase hex
    /// SHA-256 digests — one per FastCDC chunk, in file order.
    #[wasm_bindgen(js_name = createBinaryChunked)]
    pub fn create_binary_chunked(
        &self,
        path: String,
        chunk_hashes_csv: String,
        size: f64,
    ) -> Result<String, JsValue> {
        let chunk_hashes: Vec<String> = chunk_hashes_csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        with_manifest_mut(&self.manifest, |m| {
            ops::create_binary(m, &path, &chunk_hashes, size as u64)
                .map(|id| id.to_string_hyphenated())
                .map_err(to_js)
        })
    }

    pub fn delete(&self, path: String) -> Result<(), JsValue> {
        with_manifest_mut(&self.manifest, |m| ops::delete(m, &path).map_err(to_js))
    }

    pub fn rename(&self, from: String, to: String) -> Result<(), JsValue> {
        with_manifest_mut(&self.manifest, |m| ops::rename(m, &from, &to).map_err(to_js))
    }

    #[wasm_bindgen(js_name = recordModifyText)]
    pub fn record_modify_text(&self, path: String) -> Result<(), JsValue> {
        with_manifest_mut(&self.manifest, |m| {
            ops::record_modify_text(m, &path).map_err(to_js)
        })
    }

    /// Record a single-blob modification. See [`create_binary`] —
    /// thin wrapper that lifts the single hash into a length-1 chunk
    /// list. Prefer [`record_modify_binary_chunked`] in new code.
    #[wasm_bindgen(js_name = recordModifyBinary)]
    pub fn record_modify_binary(
        &self,
        path: String,
        blob_hash_hex: String,
        size: f64,
    ) -> Result<(), JsValue> {
        let chunk_hashes = vec![blob_hash_hex];
        with_manifest_mut(&self.manifest, |m| {
            ops::record_modify_binary(m, &path, &chunk_hashes, size as u64).map_err(to_js)
        })
    }

    /// Record a multi-chunk modification. `chunk_hashes_csv` is a
    /// comma-separated list of lowercase hex SHA-256 digests in file
    /// order.
    #[wasm_bindgen(js_name = recordModifyBinaryChunked)]
    pub fn record_modify_binary_chunked(
        &self,
        path: String,
        chunk_hashes_csv: String,
        size: f64,
    ) -> Result<(), JsValue> {
        let chunk_hashes: Vec<String> = chunk_hashes_csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        with_manifest_mut(&self.manifest, |m| {
            ops::record_modify_binary(m, &path, &chunk_hashes, size as u64).map_err(to_js)
        })
    }

    // ---------------------------------------------------------------
    // Manifest read API (projection)
    // ---------------------------------------------------------------

    /// Projection as JSON — array of
    /// `{id, path, kind, blob_hash, chunk_hashes, size,
    ///  is_conflict_copy}` sorted by path. Deterministic across peers.
    ///
    /// `chunk_hashes` is the canonical content addressing (always an
    /// array, even for small files where it has length 1). `blob_hash`
    /// is preserved as a backwards-compat field — it's the single hash
    /// when `chunk_hashes.length == 1`, `null` otherwise. New plugin
    /// code should consume `chunk_hashes`.
    #[wasm_bindgen(js_name = projectionJson)]
    pub fn projection_json(&self) -> Result<String, JsValue> {
        let manifest = self.manifest.borrow();
        let m = manifest
            .as_ref()
            .ok_or_else(|| JsValue::from_str("manifest not initialised"))?;
        let proj = project(m);
        let mut rows: Vec<&ProjectedEntry> = proj.by_path.values().collect();
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let single_blob: Option<&str> = r.single_blob_hash();
                serde_json::json!({
                    "id": r.id.to_string_hyphenated(),
                    "path": r.path,
                    "kind": r.kind.as_str(),
                    "blob_hash": single_blob,
                    "chunk_hashes": r.chunk_hashes,
                    "size": r.size,
                    "is_conflict_copy": r.is_conflict_copy,
                })
            })
            .collect();
        serde_json::to_string(&json).map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = projectionHashHex)]
    pub fn projection_hash_hex(&self) -> Result<String, JsValue> {
        let manifest = self.manifest.borrow();
        let m = manifest
            .as_ref()
            .ok_or_else(|| JsValue::from_str("manifest not initialised"))?;
        Ok(hex_lower(&projection_hash(m)))
    }

    /// Send a `MSG_MANIFEST_VERIFY` heartbeat. Server reply, if any,
    /// arrives as `MSG_MANIFEST_SYNC` STEP_1.
    #[wasm_bindgen(js_name = sendVerify)]
    pub fn send_verify(&self) -> Result<(), JsValue> {
        let manifest = self.manifest.borrow();
        let m = manifest
            .as_ref()
            .ok_or_else(|| JsValue::from_str("manifest not initialised"))?;
        let ws = self.ws.borrow();
        let ws = ws.as_ref().ok_or_else(|| JsValue::from_str("not connected"))?;
        let hash = projection_hash(m);
        let payload = encode_verify_payload(&hash);
        let frame = encode_message(MSG_MANIFEST_VERIFY, MANIFEST_DOC_ID, &payload);
        send_frame(ws, &frame);
        // We don't get a reply on a hash match — the server stays
        // silent. Track the in-flight state so the sidebar can show
        // "pending"; a subsequent received `MSG_MANIFEST_VERIFY` flips
        // it to match/mismatch (see handle_remote_frame).
        *self.last_verify_result.borrow_mut() = "pending";
        Ok(())
    }

    // ---------------------------------------------------------------
    // Content subdocs (text)
    // ---------------------------------------------------------------

    /// Start tracking a text content subdoc. Optional `state` rehydrates
    /// from a persisted Yrs snapshot. If connected, fires a
    /// `MSG_SYNC_STEP_1` to pull remote updates.
    #[wasm_bindgen(js_name = subscribeContent)]
    pub fn subscribe_content(
        &self,
        node_id_hex: String,
        state: Option<Vec<u8>>,
    ) -> Result<(), JsValue> {
        let node_id = NodeId::parse_str(&node_id_hex)
            .ok_or_else(|| JsValue::from_str(&format!("bad node id {node_id_hex}")))?;

        let doc = Doc::new();
        if let Some(bytes) = state.as_deref() {
            if !bytes.is_empty() {
                let update = Update::decode_v1(bytes)
                    .map_err(|e| JsValue::from_str(&format!("subscribe_content state: {e}")))?;
                let mut txn = doc.transact_mut();
                txn.apply_update(update);
            }
        }

        let is_receiving = Rc::new(RefCell::new(false));
        let is_receiving_cb = is_receiving.clone();
        let ws_cb = self.ws.clone();
        let is_connected_cb = self.is_connected.clone();
        let on_content_cb = self.on_content_changed.clone();
        let node_id_for_cb = node_id;

        let sub = doc
            .observe_update_v1(move |_, event| {
                if *is_receiving_cb.borrow() {
                    return;
                }
                if *is_connected_cb.borrow() {
                    if let Some(ws) = ws_cb.borrow().as_ref() {
                        let frame = encode_message(
                            MSG_UPDATE,
                            &content_doc_id(node_id_for_cb),
                            &event.update,
                        );
                        send_frame(ws, &frame);
                    }
                }
                let cb = on_content_cb.borrow().clone();
                if let Some(cb) = cb {
                    let _ = cb.call1(
                        &JsValue::NULL,
                        &JsValue::from_str(&node_id_for_cb.to_string_hyphenated()),
                    );
                }
            })
            .map_err(|_| JsValue::from_str("subscribe_content: observe failed"))?;

        // Snapshot the state vector while we still own `doc`.
        let sv_bytes = doc.transact().state_vector().encode_v1();

        self.content.borrow_mut().insert(
            node_id,
            ContentDoc {
                doc,
                _sub: sub,
                is_receiving,
            },
        );

        if *self.is_connected.borrow() {
            if let Some(ws) = self.ws.borrow().as_ref() {
                let frame = encode_message(MSG_SYNC_STEP_1, &content_doc_id(node_id), &sv_bytes);
                send_frame(ws, &frame);
            }
        } else {
            // WS hasn't completed handshake yet (or has disconnected
            // and not yet reconnected). Queue this subscribe; it will
            // be flushed by `onopen`. Without this, the STEP_1 is
            // silently dropped, the TS wrapper still records the node
            // as subscribed, and the content for this node never
            // arrives until a full client recreate (#57).
            self.pending_step1.borrow_mut().insert(node_id);
        }

        Ok(())
    }

    #[wasm_bindgen(js_name = unsubscribeContent)]
    pub fn unsubscribe_content(&self, node_id_hex: String) -> Result<(), JsValue> {
        let node_id = NodeId::parse_str(&node_id_hex)
            .ok_or_else(|| JsValue::from_str(&format!("bad node id {node_id_hex}")))?;
        self.content.borrow_mut().remove(&node_id);
        self.pending_step1.borrow_mut().remove(&node_id);
        Ok(())
    }

    #[wasm_bindgen(js_name = getContentText)]
    pub fn get_content_text(&self, node_id_hex: String) -> Option<String> {
        let node_id = NodeId::parse_str(&node_id_hex)?;
        let content = self.content.borrow();
        let cd = content.get(&node_id)?;
        let text = cd.doc.get_or_insert_text("text");
        let txn = cd.doc.transact();
        Some(text.get_string(&txn))
    }

    /// Diff-apply `new_content` into the subdoc's `text` Y.Text.
    #[wasm_bindgen(js_name = updateContentText)]
    pub fn update_content_text(&self, node_id_hex: String, new_content: String) {
        let Some(node_id) = NodeId::parse_str(&node_id_hex) else {
            return;
        };
        let content = self.content.borrow();
        let Some(cd) = content.get(&node_id) else {
            return;
        };
        let text = cd.doc.get_or_insert_text("text");
        let mut txn = cd.doc.transact_mut();
        let current = text.get_string(&txn);
        if current == new_content {
            return;
        }
        if current.is_empty() {
            text.insert(&mut txn, 0, &new_content);
            return;
        }
        if new_content.is_empty() {
            text.remove_range(&mut txn, 0, current.len() as u32);
            return;
        }
        let diff = dissimilar::diff(&current, &new_content);
        let mut cursor = 0u32;
        for chunk in diff {
            match chunk {
                dissimilar::Chunk::Equal(v) => cursor += v.len() as u32,
                dissimilar::Chunk::Delete(v) => {
                    text.remove_range(&mut txn, cursor, v.len() as u32);
                }
                dissimilar::Chunk::Insert(v) => {
                    text.insert(&mut txn, cursor, v);
                    cursor += v.len() as u32;
                }
            }
        }
    }

    #[wasm_bindgen(js_name = contentSnapshot)]
    pub fn content_snapshot(&self, node_id_hex: String) -> Option<Uint8Array> {
        let node_id = NodeId::parse_str(&node_id_hex)?;
        let content = self.content.borrow();
        let cd = content.get(&node_id)?;
        let txn = cd.doc.transact();
        let bytes = txn.encode_state_as_update_v1(&StateVector::default());
        Some(Uint8Array::from(&bytes[..]))
    }

    // ---------------------------------------------------------------
    // Blob protocol
    // ---------------------------------------------------------------

    /// Push a blob. Returns its hex hash.
    #[wasm_bindgen(js_name = sendBlob)]
    pub fn send_blob(&self, bytes: &[u8]) -> Result<String, JsValue> {
        let hash = hash_hex(bytes);
        if !*self.is_connected.borrow() {
            return Err(JsValue::from_str("send_blob: not connected"));
        }
        let ws = self.ws.borrow();
        let ws = ws.as_ref().ok_or_else(|| JsValue::from_str("send_blob: no socket"))?;
        let frame = encode_message(MSG_BLOB_UPDATE, &hash, bytes);
        send_frame(ws, &frame);
        // Activity feed event — the path lookup is best-effort; if
        // the blob is being pushed before its manifest entry exists,
        // we just report the hash.
        let path: Option<String> = {
            let manifest = self.manifest.borrow();
            manifest.as_ref().and_then(|m| {
                project(m)
                    .by_path
                    .iter()
                    .find(|(_, e)| e.chunk_hashes.iter().any(|h| h == &hash))
                    .map(|(p, _)| p.clone())
            })
        };
        self.handles().emit_sync_event(serde_json::json!({
            "kind": "blob_up",
            "path": path,
            "hash": hash,
            "bytes": bytes.len(),
        }));
        Ok(hash)
    }

    /// Request a blob by its hex hash. Deduplicates within a session.
    /// Reply arrives via the `onBlob` callback.
    ///
    /// Records the wall-clock instant of the send in `requested_blobs`
    /// so [`retry_stale_blob_requests`] can re-issue requests whose
    /// reply never arrived. Without that retry path, a `MSG_BLOB_REQUEST`
    /// dropped during a brief WS hiccup pinned the sidebar's "in
    /// flight" counter forever (manually reproduced on a 1.2.0 vault
    /// with two large binary chunks).
    #[wasm_bindgen(js_name = requestBlob)]
    pub fn request_blob(&self, blob_hash_hex: String) -> Result<(), JsValue> {
        if !*self.is_connected.borrow() {
            return Err(JsValue::from_str("request_blob: not connected"));
        }
        // Already-pending: no-op. Retries are the dedicated job of
        // `retry_stale_blob_requests`.
        if self
            .requested_blobs
            .borrow()
            .contains_key(&blob_hash_hex)
        {
            return Ok(());
        }
        self.requested_blobs
            .borrow_mut()
            .insert(blob_hash_hex.clone(), Date::now());
        let ws = self.ws.borrow();
        let ws = ws
            .as_ref()
            .ok_or_else(|| JsValue::from_str("request_blob: no socket"))?;
        let frame = encode_message(MSG_BLOB_REQUEST, &blob_hash_hex, blob_hash_hex.as_bytes());
        send_frame(ws, &frame);
        // Newly-pending blob — the queue grew, sidebar wants to know.
        self.handles().emit_blob_queue_change();
        Ok(())
    }

    /// Re-issue any `MSG_BLOB_REQUEST` whose last send is older than
    /// `stale_after_ms`. Returns the number of requests resent. The
    /// predicate that picks stale requests lives in
    /// [`collect_stale_blob_requests`] so it's unit-testable without
    /// a live WebSocket.
    ///
    /// This is the missing companion to [`request_blob`]'s session-
    /// dedupe set. A request frame can be dropped under WS
    /// backpressure or lost during a reconnect window. Without this
    /// retry, the dedupe set keeps the hash forever and the sidebar's
    /// "in flight" counter never zeroes out.
    ///
    /// Idempotent and safe to call frequently — entries that aren't
    /// stale are left alone; if the WebSocket isn't connected the
    /// call short-circuits with `0`. Plugin-side caller drives this
    /// on a small interval (e.g. every 5 s with a 15 s threshold).
    #[wasm_bindgen(js_name = retryStaleBlobRequests)]
    pub fn retry_stale_blob_requests(&self, stale_after_ms: f64) -> u32 {
        if !*self.is_connected.borrow() {
            return 0;
        }
        let now = Date::now();

        // Step 1: prune ghost requests — pending hashes that no live
        // manifest entry references any more (e.g. the entry's
        // chunk_hashes changed under a manifest update). Without this
        // we'd re-ask the server forever for blobs it will never
        // have.
        if let Some(m) = self.manifest.borrow().as_ref() {
            // `live_entries()` returns owned `NodeEntry`s, so we have
            // to hold the entry list ourselves to keep its
            // `chunk_hashes` strings alive while we build the ref-set.
            let entries = m.live_entries();
            let live: std::collections::HashSet<&str> = entries
                .iter()
                .flat_map(|e| e.chunk_hashes.iter().map(|s| s.as_str()))
                .collect();
            let ghosts = crate::blob_retry::collect_ghost_blob_requests(
                &self.requested_blobs.borrow(),
                &live,
            );
            if !ghosts.is_empty() {
                let mut map = self.requested_blobs.borrow_mut();
                for h in &ghosts {
                    map.remove(h);
                }
                drop(map);
                web_sys::console::warn_1(&JsValue::from_str(&format!(
                    "[SynclineV1] pruned {} ghost MSG_BLOB_REQUEST(s) — \
                     hashes are no longer in any manifest entry",
                    ghosts.len()
                )));
                self.handles().emit_blob_queue_change();
            }
        }

        // Step 2: re-send anything past the stale threshold.
        let stale = crate::blob_retry::collect_stale_blob_requests(
            &self.requested_blobs.borrow(),
            now,
            stale_after_ms,
        );
        if stale.is_empty() {
            return 0;
        }
        let ws = self.ws.borrow();
        let Some(ws) = ws.as_ref() else {
            return 0;
        };
        // Bump every stale timestamp first so a follow-up call
        // before the reply can settle won't double-send.
        {
            let mut map = self.requested_blobs.borrow_mut();
            for h in &stale {
                map.insert(h.clone(), now);
            }
        }
        for hash in &stale {
            let frame = encode_message(MSG_BLOB_REQUEST, hash, hash.as_bytes());
            send_frame(ws, &frame);
        }
        web_sys::console::warn_1(&JsValue::from_str(&format!(
            "[SynclineV1] re-sent {} stale MSG_BLOB_REQUEST frame(s)",
            stale.len()
        )));
        stale.len() as u32
    }

    // ---------------------------------------------------------------
    // Shared handles for the message dispatcher
    // ---------------------------------------------------------------

    fn handles(&self) -> Handles {
        Handles {
            manifest: self.manifest.clone(),
            manifest_is_receiving: self.manifest_is_receiving.clone(),
            content: self.content.clone(),
            requested_blobs: self.requested_blobs.clone(),
            last_verify_result: self.last_verify_result.clone(),
            on_manifest_changed: self.on_manifest_changed.clone(),
            on_content_changed: self.on_content_changed.clone(),
            on_blob: self.on_blob.clone(),
            on_blob_queue_change: self.on_blob_queue_change.clone(),
            on_sync_event: self.on_sync_event.clone(),
            on_server_stats: self.on_server_stats.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Incoming-frame dispatcher
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Handles {
    manifest: Rc<RefCell<Option<Manifest>>>,
    manifest_is_receiving: Rc<RefCell<bool>>,
    content: Rc<RefCell<HashMap<NodeId, ContentDoc>>>,
    requested_blobs: Rc<RefCell<HashMap<String, f64>>>,
    last_verify_result: Rc<RefCell<&'static str>>,
    on_manifest_changed: Rc<RefCell<Option<Function>>>,
    on_content_changed: Rc<RefCell<Option<Function>>>,
    on_blob: Rc<RefCell<Option<Function>>>,
    on_blob_queue_change: Rc<RefCell<Option<Function>>>,
    on_sync_event: Rc<RefCell<Option<Function>>>,
    on_server_stats: Rc<RefCell<Option<Function>>>,
}

impl Handles {
    /// Fire `on_blob_queue_change(pendingCount, pendingBytes)` if the
    /// callback is registered. Called after every insert/remove on
    /// `requested_blobs`.
    fn emit_blob_queue_change(&self) {
        let Some(cb) = self.on_blob_queue_change.borrow().clone() else {
            return;
        };
        let count = self.requested_blobs.borrow().len() as u32;
        // Approximate the byte count the same way the synchronous
        // getter does — see `pending_blob_bytes`.
        let bytes: u64 = {
            let manifest = self.manifest.borrow();
            let pending = self.requested_blobs.borrow();
            match manifest.as_ref() {
                Some(m) if !pending.is_empty() => m
                    .live_entries()
                    .into_iter()
                    .filter(|e| e.chunk_hashes.iter().any(|h| pending.contains_key(h)))
                    .map(|e| e.size)
                    .sum(),
                _ => 0,
            }
        };
        let _ = cb.call2(
            &JsValue::NULL,
            &JsValue::from_f64(count as f64),
            &JsValue::from_f64(bytes as f64),
        );
    }

    /// Fire `on_sync_event(jsonString)` for the activity feed.
    /// `payload` is a `serde_json::Value` so callers don't have to
    /// hand-format. We marshal as a string rather than a `JsValue`
    /// object because the boundary is simpler — the plugin runs
    /// `JSON.parse` on the other side.
    fn emit_sync_event(&self, payload: serde_json::Value) {
        let Some(cb) = self.on_sync_event.borrow().clone() else {
            return;
        };
        let s = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = cb.call1(&JsValue::NULL, &JsValue::from_str(&s));
    }
}

fn dispatch_frame(h: &Handles, ws: &WebSocket, data: &[u8]) {
    let Some((msg_type, doc_id, payload)) = decode_message(data) else {
        web_sys::console::error_1(&JsValue::from_str("[SynclineV1] malformed frame"));
        return;
    };

    match msg_type {
        MSG_VERSION => {
            if let Some((major, minor)) = decode_version_handshake(payload) {
                web_sys::console::log_1(&JsValue::from_str(&format!(
                    "[SynclineV1] server v{major}.{minor}"
                )));
                let frame = {
                    let manifest = h.manifest.borrow();
                    let Some(m) = manifest.as_ref() else {
                        return;
                    };
                    let payload = manifest_step1_payload(m);
                    encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &payload)
                };
                send_frame(ws, &frame);
            }
        }
        MSG_MANIFEST_SYNC => {
            // Apply the incoming manifest sync under the is_receiving
            // guard so the observer doesn't re-broadcast the remote's
            // own bytes back out.
            let response = {
                let mut manifest = h.manifest.borrow_mut();
                let Some(m) = manifest.as_mut() else {
                    return;
                };
                *h.manifest_is_receiving.borrow_mut() = true;
                let result = handle_manifest_payload(m, payload);
                *h.manifest_is_receiving.borrow_mut() = false;
                result
            };
            match response {
                Ok(Some(resp)) => {
                    let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &resp);
                    send_frame(ws, &frame);
                }
                Ok(None) => {}
                Err(e) => {
                    web_sys::console::error_1(&JsValue::from_str(&format!(
                        "[SynclineV1] manifest handle error: {e}"
                    )));
                    return;
                }
            }
            let cb = h.on_manifest_changed.borrow().clone();
            if let Some(cb) = cb {
                let _ = cb.call0(&JsValue::NULL);
            }
        }
        MSG_MANIFEST_VERIFY => {
            let Some(remote) = decode_verify_payload(payload) else {
                return;
            };
            let frame_opt = {
                let manifest = h.manifest.borrow();
                let Some(m) = manifest.as_ref() else {
                    return;
                };
                let local = projection_hash(m);
                if local == remote {
                    *h.last_verify_result.borrow_mut() = "match";
                    None
                } else {
                    *h.last_verify_result.borrow_mut() = "mismatch";
                    let payload = manifest_step1_payload(m);
                    Some(encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &payload))
                }
            };
            if let Some(f) = frame_opt {
                send_frame(ws, &f);
            }
        }
        MSG_SYNC_STEP_2 | MSG_UPDATE => {
            let Some(node_id) = parse_content_doc_id(doc_id) else {
                return;
            };
            let Ok(update) = Update::decode_v1(payload) else {
                web_sys::console::error_1(&JsValue::from_str(
                    "[SynclineV1] bad content update bytes",
                ));
                return;
            };
            {
                let content = h.content.borrow();
                let Some(cd) = content.get(&node_id) else {
                    return;
                };
                *cd.is_receiving.borrow_mut() = true;
                {
                    let mut txn = cd.doc.transact_mut();
                    txn.apply_update(update);
                }
                *cd.is_receiving.borrow_mut() = false;
            }
            let cb = h.on_content_changed.borrow().clone();
            if let Some(cb) = cb {
                let _ = cb.call1(
                    &JsValue::NULL,
                    &JsValue::from_str(&node_id.to_string_hyphenated()),
                );
            }
        }
        MSG_SYNC_STEP_1 => {
            let Some(node_id) = parse_content_doc_id(doc_id) else {
                return;
            };
            let Ok(remote_sv) = StateVector::decode_v1(payload) else {
                return;
            };
            let frame_opt = {
                let content = h.content.borrow();
                let Some(cd) = content.get(&node_id) else {
                    return;
                };
                let update = cd.doc.transact().encode_state_as_update_v1(&remote_sv);
                Some(encode_message(MSG_SYNC_STEP_2, doc_id, &update))
            };
            if let Some(f) = frame_opt {
                send_frame(ws, &f);
            }
        }
        MSG_BLOB_UPDATE => {
            // Verify hash before surfacing bytes to JS.
            let expected = doc_id.to_string();
            if payload.is_empty() {
                return;
            }
            let actual = hash_hex(payload);
            if actual != expected {
                web_sys::console::error_1(&JsValue::from_str(&format!(
                    "[SynclineV1] blob hash mismatch: expected {expected}, got {actual}"
                )));
                return;
            }
            let was_pending = h.requested_blobs.borrow_mut().remove(&expected).is_some();
            let cb = h.on_blob.borrow().clone();
            if let Some(cb) = cb {
                let js_hash = JsValue::from_str(&expected);
                let js_bytes = Uint8Array::from(payload);
                let _ = cb.call2(&JsValue::NULL, &js_hash, &js_bytes);
            }
            // Fire sidebar hooks. `was_pending` filters out
            // server-pushed broadcasts that we never explicitly asked
            // for — those still hit the on_blob callback (because
            // they're real new content) but they don't change the
            // pending-queue progress UI.
            if was_pending {
                h.emit_blob_queue_change();
            }
            // Find the user-facing path for this blob, if any. The
            // activity feed wants "↓ note.png", not a hex hash.
            let path: Option<String> = {
                let manifest = h.manifest.borrow();
                manifest.as_ref().and_then(|m| {
                    project(m)
                        .by_path
                        .iter()
                        .find(|(_, e)| e.chunk_hashes.iter().any(|h| h == &expected))
                        .map(|(p, _)| p.clone())
                })
            };
            h.emit_sync_event(serde_json::json!({
                "kind": "blob_down",
                "path": path,
                "hash": expected,
                "bytes": payload.len(),
            }));
        }
        MSG_SERVER_STATS => {
            // Pass the JSON straight through — the plugin owns
            // rendering. Old servers that don't recognise this opcode
            // simply never reply, and the plugin's "—" placeholders
            // stay in the UI.
            if let Some(cb) = h.on_server_stats.borrow().clone() {
                let s = match std::str::from_utf8(payload) {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        web_sys::console::warn_1(&JsValue::from_str(
                            "[SynclineV1] MSG_SERVER_STATS payload was not utf-8",
                        ));
                        return;
                    }
                };
                let _ = cb.call1(&JsValue::NULL, &JsValue::from_str(&s));
            }
        }
        _ => {
            web_sys::console::warn_1(&JsValue::from_str(&format!(
                "[SynclineV1] unexpected msg_type {msg_type:#x} for {doc_id}"
            )));
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_frame(ws: &WebSocket, bytes: &[u8]) {
    let array = Uint8Array::from(bytes);
    if let Err(e) = ws.send_with_array_buffer_view(&array) {
        web_sys::console::error_2(&JsValue::from_str("[SynclineV1] send failed"), &e);
    }
}

fn content_doc_id(node_id: NodeId) -> String {
    format!("content:{}", node_id.to_string_hyphenated())
}

fn parse_content_doc_id(doc_id: &str) -> Option<NodeId> {
    let rest = doc_id.strip_prefix("content:")?;
    NodeId::parse_str(rest)
}

fn fire_status(cb: &Rc<RefCell<Option<Function>>>, s: &str) {
    let cb = cb.borrow().clone();
    if let Some(f) = cb {
        let _ = f.call1(&JsValue::NULL, &JsValue::from_str(s));
    }
}

fn to_js(e: anyhow::Error) -> JsValue {
    JsValue::from_str(&format!("{e:#}"))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn with_manifest_mut<T, F>(
    manifest: &Rc<RefCell<Option<Manifest>>>,
    f: F,
) -> Result<T, JsValue>
where
    F: FnOnce(&mut Manifest) -> Result<T, JsValue>,
{
    let mut m = manifest.borrow_mut();
    let m = m
        .as_mut()
        .ok_or_else(|| JsValue::from_str("manifest not initialised"))?;
    f(m)
}

