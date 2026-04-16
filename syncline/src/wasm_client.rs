use js_sys::{Function, Uint8Array};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Any, Doc, GetString, Map, Out, ReadTxn, StateVector, Subscription, Text, Transact, Update};

use crate::protocol::{
    decode_message, encode_message, MSG_BLOB_REQUEST, MSG_BLOB_UPDATE, MSG_RESYNC,
    MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE,
};

struct DocState {
    doc: Doc,
    callback: Function,
    blob_callback: Option<Function>,
    _sub: Subscription,
    is_receiving: Rc<RefCell<bool>>,
    doc_type: DocType,
}

enum DocType {
    Text,
    Map,
}

#[wasm_bindgen]
pub struct SynclineClient {
    url: String,
    ws: Option<WebSocket>,
    docs: Rc<RefCell<HashMap<String, DocState>>>,
    closures: Rc<RefCell<Vec<Closure<dyn FnMut(JsValue)>>>>,
    is_connected: Rc<RefCell<bool>>,
    index_callback: Rc<RefCell<Option<Function>>>,
    resync_interval_id: Rc<RefCell<Option<i32>>>,
}

#[wasm_bindgen]
impl SynclineClient {
    #[wasm_bindgen(constructor)]
    pub fn new(url: String) -> Result<SynclineClient, JsValue> {
        console_error_panic_hook::set_once();

        Ok(SynclineClient {
            url,
            ws: None,
            docs: Rc::new(RefCell::new(HashMap::new())),
            closures: Rc::new(RefCell::new(Vec::new())),
            is_connected: Rc::new(RefCell::new(false)),
            index_callback: Rc::new(RefCell::new(None)),
            resync_interval_id: Rc::new(RefCell::new(None)),
        })
    }

    pub fn connect(&mut self) -> Result<(), JsValue> {
        let ws = WebSocket::new(&self.url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let docs_clone = self.docs.clone();
        let is_connected_open = self.is_connected.clone();
        let is_connected_err = self.is_connected.clone();
        let is_connected_close = self.is_connected.clone();
        let url_err = self.url.clone();
        let ws_send = ws.clone();

        // ON OPEN
        let docs_on_open = self.docs.clone();
        let onopen = Closure::wrap(Box::new(move |_| {
            *is_connected_open.borrow_mut() = true;
            web_sys::console::log_1(&JsValue::from_str("[Syncline] Connected"));

            let docs = docs_on_open.borrow();
            for (doc_id, state) in docs.iter() {
                // Send SYNC_STEP_1 to request remote state
                let sv = state.doc.transact().state_vector().encode_v1();
                let msg = encode_message(MSG_SYNC_STEP_1, doc_id, &sv);
                let array = Uint8Array::from(&msg[..]);
                if let Err(e) = ws_send.send_with_array_buffer_view(&array) {
                    web_sys::console::error_1(&e);
                }

                // Also send current state as UPDATE
                let txn = state.doc.transact();
                let update = txn.encode_state_as_update_v1(&yrs::StateVector::default());
                if !update.is_empty() {
                    let msg = encode_message(MSG_UPDATE, doc_id, &update);
                    let array = Uint8Array::from(&msg[..]);
                    if let Err(e) = ws_send.send_with_array_buffer_view(&array) {
                        web_sys::console::error_1(&e);
                    }
                }
            }
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onopen);

        // ON MESSAGE
        let index_callback = self.index_callback.clone();
        let onmessage = Closure::wrap(Box::new(move |val: JsValue| {
            let e = val.unchecked_into::<MessageEvent>();
            if let Ok(ab) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = Uint8Array::new(&ab);
                let data = array.to_vec();

                if let Some((msg_type, doc_id, payload)) = decode_message(&data) {
                    // Handle index document specially
                    if doc_id == "__index__" {
                        if let Ok(u) = Update::decode_v1(payload) {
                            let callback_opt = {
                                let docs = docs_clone.borrow();
                                if let Some(state) = docs.get(doc_id) {
                                    *state.is_receiving.borrow_mut() = true;
                                    {
                                        let mut txn = state.doc.transact_mut();
                                        txn.apply_update(u);
                                    }
                                    *state.is_receiving.borrow_mut() = false;
                                }
                                index_callback.borrow().clone()
                            };

                            if let Some(cb) = callback_opt {
                                let _ = cb.call0(&JsValue::NULL);
                            }
                        }
                    } else {
                        let callback_opt = {
                            let docs = docs_clone.borrow();
                            if let Some(state) = docs.get(doc_id) {
                                match msg_type {
                                    MSG_SYNC_STEP_2 | MSG_UPDATE => {
                                        if let Ok(u) = Update::decode_v1(payload) {
                                            *state.is_receiving.borrow_mut() = true;
                                            {
                                                let mut txn = state.doc.transact_mut();
                                                txn.apply_update(u);
                                            }
                                            *state.is_receiving.borrow_mut() = false;
                                            Some((state.callback.clone(), None))
                                        } else {
                                            None
                                        }
                                    }
                                    MSG_BLOB_UPDATE => {
                                        // Binary blob received — invoke blob_callback with doc_id and data
                                        if let Some(ref blob_cb) = state.blob_callback {
                                            Some((blob_cb.clone(), Some((doc_id.to_string(), payload.to_vec()))))
                                        } else {
                                            None
                                        }
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        };

                        if let Some((cb, blob_info)) = callback_opt {
                            if let Some((blob_doc_id, blob_data)) = blob_info {
                                // Call blob callback with (doc_id, Uint8Array)
                                let js_doc_id = JsValue::from_str(&blob_doc_id);
                                let js_data = Uint8Array::from(&blob_data[..]);
                                let _ = cb.call2(&JsValue::NULL, &js_doc_id, &js_data);
                            } else {
                                let _ = cb.call0(&JsValue::NULL);
                            }
                        }
                    }
                }
            }
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onmessage);

        // ON ERROR
        let onerror = Closure::wrap(Box::new(move |val: JsValue| {
            *is_connected_err.borrow_mut() = false;
            web_sys::console::error_2(
                &JsValue::from_str(&format!("[Syncline] WebSocket error: {}", url_err)),
                &val,
            );
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onerror);

        // ON CLOSE
        let onclose = Closure::wrap(Box::new(move |_| {
            *is_connected_close.borrow_mut() = false;
            web_sys::console::log_1(&JsValue::from_str("[Syncline] Disconnected"));
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        self.closures.borrow_mut().push(onclose);

        self.ws = Some(ws.clone());

        // Start periodic resync interval (every 60s)
        self.stop_resync_interval();
        let docs_resync = self.docs.clone();
        let is_connected_resync = self.is_connected.clone();
        let ws_resync = ws;
        let resync_cb = Closure::wrap(Box::new(move || {
            if !*is_connected_resync.borrow() {
                return;
            }
            let docs = docs_resync.borrow();
            for (doc_id, state) in docs.iter() {
                let sv = state.doc.transact().state_vector().encode_v1();
                let msg = encode_message(MSG_RESYNC, doc_id, &sv);
                let array = Uint8Array::from(&msg[..]);
                let _ = ws_resync.send_with_array_buffer_view(&array);
            }
        }) as Box<dyn FnMut()>);

        let window = web_sys::window().unwrap();
        let id = window
            .set_interval_with_callback_and_timeout_and_arguments_0(
                resync_cb.as_ref().unchecked_ref(),
                60_000, // 60 seconds
            )
            .unwrap_or(-1);
        *self.resync_interval_id.borrow_mut() = Some(id);
        // Prevent the closure from being dropped (which would invalidate the callback)
        resync_cb.forget();

        Ok(())
    }

    fn stop_resync_interval(&self) {
        if let Some(id) = self.resync_interval_id.borrow_mut().take() {
            if let Some(window) = web_sys::window() {
                window.clear_interval_with_handle(id);
            }
        }
    }

    pub fn add_doc(&self, doc_id: String, callback: Function) -> Result<(), JsValue> {
        let doc = Doc::new();
        let is_receiving = Rc::new(RefCell::new(false));

        let ws_clone = self.ws.clone();
        let doc_id_clone = doc_id.clone();
        let is_receiving_clone = is_receiving.clone();
        let is_connected_send = self.is_connected.clone();

        let sub = doc
            .observe_update_v1(move |_, event| {
                if *is_receiving_clone.borrow() {
                    return;
                }
                if !*is_connected_send.borrow() {
                    return;
                }
                if let Some(ref ws) = ws_clone {
                    let msg = encode_message(MSG_UPDATE, &doc_id_clone, &event.update);
                    let array = Uint8Array::from(&msg[..]);
                    let _ = ws.send_with_array_buffer_view(&array);
                }
            })
            .map_err(|_| JsValue::from_str("Failed to subscribe to doc"))?;

        self.docs.borrow_mut().insert(
            doc_id.clone(),
            DocState {
                doc: doc.clone(),
                callback,
                blob_callback: None,
                _sub: sub,
                is_receiving,
                doc_type: DocType::Text,
            },
        );

        if *self.is_connected.borrow() {
            if let Some(ref ws) = self.ws {
                let sv = doc.transact().state_vector().encode_v1();
                let msg = encode_message(MSG_SYNC_STEP_1, &doc_id, &sv);
                let array = Uint8Array::from(&msg[..]);
                ws.send_with_array_buffer_view(&array)?;
            }
        }

        Ok(())
    }

    pub fn create_index(&self, callback: Option<Function>) -> Result<(), JsValue> {
        let doc = Doc::new();
        doc.get_or_insert_text("content");
        let is_receiving = Rc::new(RefCell::new(false));

        if let Some(cb) = callback {
            *self.index_callback.borrow_mut() = Some(cb);
        }

        let ws_clone = self.ws.clone();
        let is_receiving_clone = is_receiving.clone();
        let is_connected_send = self.is_connected.clone();
        let doc_id = "__index__".to_string();

        let sub = doc
            .observe_update_v1(move |_, event| {
                if *is_receiving_clone.borrow() {
                    return;
                }
                if !*is_connected_send.borrow() {
                    return;
                }
                if let Some(ref ws) = ws_clone {
                    let msg = encode_message(MSG_UPDATE, &doc_id, &event.update);
                    let array = Uint8Array::from(&msg[..]);
                    let _ = ws.send_with_array_buffer_view(&array);
                }
            })
            .map_err(|_| JsValue::from_str("Failed to subscribe to index"))?;

        let dummy_callback = js_sys::Function::new_no_args("");

        self.docs.borrow_mut().insert(
            "__index__".to_string(),
            DocState {
                doc,
                callback: dummy_callback,
                blob_callback: None,
                _sub: sub,
                is_receiving,
                doc_type: DocType::Text,
            },
        );

        if *self.is_connected.borrow() {
            if let Some(ref ws) = self.ws {
                let docs = self.docs.borrow();
                if let Some(state) = docs.get("__index__") {
                    let sv = state.doc.transact().state_vector().encode_v1();
                    let msg = encode_message(MSG_SYNC_STEP_1, "__index__", &sv);
                    let array = Uint8Array::from(&msg[..]);
                    ws.send_with_array_buffer_view(&array)?;
                }
            }
        }

        Ok(())
    }

    pub fn index_insert(&self, key: String) -> Result<(), JsValue> {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get("__index__") {
            let text = state.doc.get_or_insert_text("content");
            let mut txn = state.doc.transact_mut();
            let current = text.get_string(&txn);
            let target = format!("{}\n", key);
            if !current.contains(&target) {
                let len = current.len() as u32;
                text.insert(&mut txn, len, &target);
            }
        }
        Ok(())
    }

    pub fn index_remove(&self, key: &str) -> Result<(), JsValue> {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get("__index__") {
            let text = state.doc.get_or_insert_text("content");
            let mut txn = state.doc.transact_mut();
            let current = text.get_string(&txn);
            let target = format!("{}\n", key);
            if let Some(idx) = current.find(&target) {
                text.remove_range(&mut txn, idx as u32, target.len() as u32);
            }
        }
        Ok(())
    }

    pub fn index_keys(&self) -> Vec<String> {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get("__index__") {
            let text = state.doc.get_or_insert_text("content");
            let txn = state.doc.transact();
            let content = text.get_string(&txn);
            content
                .lines()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn remove_doc(&self, doc_id: &str) {
        self.docs.borrow_mut().remove(doc_id);
    }

    pub fn get_text(&self, doc_id: &str) -> Option<String> {
        let docs = self.docs.borrow();
        docs.get(doc_id).map(|state| {
            let text = state.doc.get_or_insert_text("content");
            let txn = state.doc.transact();
            text.get_string(&txn)
        })
    }

    pub fn update(&self, doc_id: &str, new_content: String) {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get(doc_id) {
            let text = state.doc.get_or_insert_text("content");
            let mut txn = state.doc.transact_mut();
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

            let mut cursor = 0;

            for chunk in diff {
                match chunk {
                    dissimilar::Chunk::Equal(val) => {
                        cursor += val.len() as u32;
                    }
                    dissimilar::Chunk::Delete(val) => {
                        text.remove_range(&mut txn, cursor, val.len() as u32);
                    }
                    dissimilar::Chunk::Insert(val) => {
                        text.insert(&mut txn, cursor, val);
                        cursor += val.len() as u32;
                    }
                }
            }
        }
    }

    pub fn set_text(&self, doc_id: &str, content: String) {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get(doc_id) {
            let text = state.doc.get_or_insert_text("content");
            let mut txn = state.doc.transact_mut();
            let len = text.len(&txn);
            if len > 0 {
                text.remove_range(&mut txn, 0, len);
            }
            text.insert(&mut txn, 0, &content);
        }
    }

    pub fn is_connected(&self) -> bool {
        *self.is_connected.borrow()
    }

    pub fn doc_count(&self) -> usize {
        self.docs.borrow().len()
    }

    /// Read the `meta.path` value from a doc's Y.Map, returning the file path
    /// that this UUID corresponds to (e.g. `"notes/idea.md"`).
    pub fn get_meta_path(&self, doc_id: &str) -> Option<String> {
        let docs = self.docs.borrow();
        docs.get(doc_id).and_then(|state| {
            let meta = state.doc.get_or_insert_map("meta");
            let txn = state.doc.transact();
            match meta.get(&txn, "path") {
                Some(Out::Any(Any::String(arc))) => Some(arc.to_string()),
                _ => None,
            }
        })
    }

    /// Write a new `meta.path` value into the doc's Y.Map.
    /// The doc's `observe_update_v1` subscription auto-broadcasts the change.
    pub fn set_meta_path(&self, doc_id: &str, path: String) {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get(doc_id) {
            let meta = state.doc.get_or_insert_map("meta");
            let mut txn = state.doc.transact_mut();
            meta.insert(&mut txn, "path", path.as_str());
        }
    }

    // -----------------------------------------------------------------------
    // Binary file helpers
    // -----------------------------------------------------------------------

    /// Add a binary document (Y.Map("meta") only, no Y.Text("content")).
    /// The `callback` fires on CRDT metadata updates; `blob_callback` fires
    /// when a `MSG_BLOB_UPDATE` arrives with the raw binary data.
    pub fn add_binary_doc(
        &self,
        doc_id: String,
        callback: Function,
        blob_callback: Function,
    ) -> Result<(), JsValue> {
        let doc = Doc::new();
        let is_receiving = Rc::new(RefCell::new(false));

        let ws_clone = self.ws.clone();
        let doc_id_clone = doc_id.clone();
        let is_receiving_clone = is_receiving.clone();
        let is_connected_send = self.is_connected.clone();

        let sub = doc
            .observe_update_v1(move |_, event| {
                if *is_receiving_clone.borrow() {
                    return;
                }
                if !*is_connected_send.borrow() {
                    return;
                }
                if let Some(ref ws) = ws_clone {
                    let msg = encode_message(MSG_UPDATE, &doc_id_clone, &event.update);
                    let array = Uint8Array::from(&msg[..]);
                    let _ = ws.send_with_array_buffer_view(&array);
                }
            })
            .map_err(|_| JsValue::from_str("Failed to subscribe to binary doc"))?;

        self.docs.borrow_mut().insert(
            doc_id.clone(),
            DocState {
                doc: doc.clone(),
                callback,
                blob_callback: Some(blob_callback),
                _sub: sub,
                is_receiving,
                doc_type: DocType::Map,
            },
        );

        if *self.is_connected.borrow() {
            if let Some(ref ws) = self.ws {
                let sv = doc.transact().state_vector().encode_v1();
                let msg = encode_message(MSG_SYNC_STEP_1, &doc_id, &sv);
                let array = Uint8Array::from(&msg[..]);
                ws.send_with_array_buffer_view(&array)?;
            }
        }

        Ok(())
    }

    /// Read `meta.type` from a doc's Y.Map.
    pub fn get_meta_type(&self, doc_id: &str) -> Option<String> {
        let docs = self.docs.borrow();
        docs.get(doc_id).and_then(|state| {
            let meta = state.doc.get_or_insert_map("meta");
            let txn = state.doc.transact();
            match meta.get(&txn, "type") {
                Some(Out::Any(Any::String(arc))) => Some(arc.to_string()),
                _ => None,
            }
        })
    }

    /// Write `meta.type` into the doc's Y.Map.
    pub fn set_meta_type(&self, doc_id: &str, file_type: String) {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get(doc_id) {
            let meta = state.doc.get_or_insert_map("meta");
            let mut txn = state.doc.transact_mut();
            meta.insert(&mut txn, "type", file_type.as_str());
        }
    }

    /// Read `meta.blob_hash` from a doc's Y.Map.
    pub fn get_blob_hash(&self, doc_id: &str) -> Option<String> {
        let docs = self.docs.borrow();
        docs.get(doc_id).and_then(|state| {
            let meta = state.doc.get_or_insert_map("meta");
            let txn = state.doc.transact();
            match meta.get(&txn, "blob_hash") {
                Some(Out::Any(Any::String(arc))) => Some(arc.to_string()),
                _ => None,
            }
        })
    }

    /// Write `meta.blob_hash` into the doc's Y.Map.
    pub fn set_blob_hash(&self, doc_id: &str, hash: String) {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get(doc_id) {
            let meta = state.doc.get_or_insert_map("meta");
            let mut txn = state.doc.transact_mut();
            meta.insert(&mut txn, "blob_hash", hash.as_str());
        }
    }

    /// Send a binary blob update to the server for the given doc.
    pub fn send_blob(&self, doc_id: &str, data: &[u8]) {
        if let Some(ref ws) = self.ws {
            if *self.is_connected.borrow() {
                let msg = encode_message(MSG_BLOB_UPDATE, doc_id, data);
                let array = Uint8Array::from(&msg[..]);
                let _ = ws.send_with_array_buffer_view(&array);
            }
        }
    }

    /// Request a blob by its SHA256 hash from the server.
    pub fn request_blob(&self, doc_id: &str, hash: &str) {
        if let Some(ref ws) = self.ws {
            if *self.is_connected.borrow() {
                let msg = encode_message(MSG_BLOB_REQUEST, doc_id, hash.as_bytes());
                let array = Uint8Array::from(&msg[..]);
                let _ = ws.send_with_array_buffer_view(&array);
            }
        }
    }

    pub fn disconnect(&mut self) {
        self.stop_resync_interval();
        if let Some(ws) = self.ws.take() {
            ws.set_onopen(None);
            ws.set_onmessage(None);
            ws.set_onerror(None);
            ws.set_onclose(None);
            let _ = ws.close();
        }
        self.closures.borrow_mut().clear();
        self.docs.borrow_mut().clear();
        *self.is_connected.borrow_mut() = false;
    }
}
