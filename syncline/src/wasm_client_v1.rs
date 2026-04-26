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

use js_sys::{Function, Uint8Array};
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
    MSG_MANIFEST_SYNC, MSG_MANIFEST_VERIFY, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE,
    MSG_VERSION,
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
    requested_blobs: Rc<RefCell<HashSet<String>>>,
    /// Content node ids whose STEP_1 was deferred because the WebSocket
    /// hadn't completed its handshake yet. Drained on `onopen`. Without
    /// this, `subscribeContent` calls during the connect() → onopen
    /// gap would silently no-op and the matching node would never see
    /// remote updates (#57).
    pending_step1: Rc<RefCell<HashSet<NodeId>>>,
    on_manifest_changed: Rc<RefCell<Option<Function>>>,
    on_content_changed: Rc<RefCell<Option<Function>>>,
    on_blob: Rc<RefCell<Option<Function>>>,
    on_status: Rc<RefCell<Option<Function>>>,
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
            requested_blobs: Rc::new(RefCell::new(HashSet::new())),
            pending_step1: Rc::new(RefCell::new(HashSet::new())),
            on_manifest_changed: Rc::new(RefCell::new(None)),
            on_content_changed: Rc::new(RefCell::new(None)),
            on_blob: Rc::new(RefCell::new(None)),
            on_status: Rc::new(RefCell::new(None)),
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

    #[wasm_bindgen(js_name = createBinary)]
    pub fn create_binary(
        &self,
        path: String,
        blob_hash_hex: String,
        size: f64,
    ) -> Result<String, JsValue> {
        with_manifest_mut(&self.manifest, |m| {
            ops::create_binary(m, &path, &blob_hash_hex, size as u64)
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

    #[wasm_bindgen(js_name = recordModifyBinary)]
    pub fn record_modify_binary(
        &self,
        path: String,
        blob_hash_hex: String,
        size: f64,
    ) -> Result<(), JsValue> {
        with_manifest_mut(&self.manifest, |m| {
            ops::record_modify_binary(m, &path, &blob_hash_hex, size as u64).map_err(to_js)
        })
    }

    // ---------------------------------------------------------------
    // Manifest read API (projection)
    // ---------------------------------------------------------------

    /// Projection as JSON — array of `{id, path, kind, blob_hash,
    /// size, is_conflict_copy}` sorted by path. Deterministic across
    /// peers.
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
                serde_json::json!({
                    "id": r.id.to_string_hyphenated(),
                    "path": r.path,
                    "kind": r.kind.as_str(),
                    "blob_hash": r.blob_hash,
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
        Ok(hash)
    }

    /// Request a blob by its hex hash. Deduplicates within a session.
    /// Reply arrives via the `onBlob` callback.
    #[wasm_bindgen(js_name = requestBlob)]
    pub fn request_blob(&self, blob_hash_hex: String) -> Result<(), JsValue> {
        if !*self.is_connected.borrow() {
            return Err(JsValue::from_str("request_blob: not connected"));
        }
        if !self.requested_blobs.borrow_mut().insert(blob_hash_hex.clone()) {
            return Ok(());
        }
        let ws = self.ws.borrow();
        let ws = ws
            .as_ref()
            .ok_or_else(|| JsValue::from_str("request_blob: no socket"))?;
        let frame = encode_message(MSG_BLOB_REQUEST, &blob_hash_hex, blob_hash_hex.as_bytes());
        send_frame(ws, &frame);
        Ok(())
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
            on_manifest_changed: self.on_manifest_changed.clone(),
            on_content_changed: self.on_content_changed.clone(),
            on_blob: self.on_blob.clone(),
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
    requested_blobs: Rc<RefCell<HashSet<String>>>,
    on_manifest_changed: Rc<RefCell<Option<Function>>>,
    on_content_changed: Rc<RefCell<Option<Function>>>,
    on_blob: Rc<RefCell<Option<Function>>>,
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
                    None
                } else {
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
            h.requested_blobs.borrow_mut().remove(&expected);
            let cb = h.on_blob.borrow().clone();
            if let Some(cb) = cb {
                let js_hash = JsValue::from_str(&expected);
                let js_bytes = Uint8Array::from(payload);
                let _ = cb.call2(&JsValue::NULL, &js_hash, &js_bytes);
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
