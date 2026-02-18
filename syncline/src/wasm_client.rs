use js_sys::{Function, Uint8Array};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, Map, ReadTxn, StateVector, Subscription, Text, Transact, Update};

use crate::protocol::{
    decode_message, encode_message, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE,
};

struct DocState {
    doc: Doc,
    callback: Function,
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
                                            Some(state.callback.clone())
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

                        if let Some(cb) = callback_opt {
                            let _ = cb.call0(&JsValue::NULL);
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

        self.ws = Some(ws);
        Ok(())
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
        let map = doc.get_or_insert_map("files");
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
                _sub: sub,
                is_receiving,
                doc_type: DocType::Map,
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
            let map = state.doc.get_or_insert_map("files");
            let mut txn = state.doc.transact_mut();
            map.insert(&mut txn, key, "1");
        }
        Ok(())
    }

    pub fn index_remove(&self, key: &str) -> Result<(), JsValue> {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get("__index__") {
            let map = state.doc.get_or_insert_map("files");
            let mut txn = state.doc.transact_mut();
            map.remove(&mut txn, key);
        }
        Ok(())
    }

    pub fn index_keys(&self) -> Vec<String> {
        let docs = self.docs.borrow();
        if let Some(state) = docs.get("__index__") {
            let map = state.doc.get_or_insert_map("files");
            let txn = state.doc.transact();
            map.keys(&txn).map(|k| k.to_string()).collect()
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

            let diffs = diff::chars(&current, &new_content);
            let mut index = 0u32;

            for d in diffs {
                match d {
                    diff::Result::Left(_) => {
                        text.remove_range(&mut txn, index, 1);
                    }
                    diff::Result::Right(r) => {
                        let s = r.to_string();
                        text.insert(&mut txn, index, &s);
                        index += 1;
                    }
                    diff::Result::Both(_, _) => {
                        index += 1;
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
}
