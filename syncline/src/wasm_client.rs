use js_sys::{Function, Uint8Array};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, Subscription, Text, Transact, Update};

use crate::protocol::{MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE};

#[wasm_bindgen]
pub struct SynclineFile {
    url: String,
    doc: Doc,
    ws: Option<WebSocket>,
    // We need to keep closures alive
    _closures: Rc<RefCell<Vec<Closure<dyn FnMut(JsValue)>>>>,
    // Keep subscription alive
    _sub: Option<Subscription>,
    callback: Function,
    is_synced: Rc<RefCell<bool>>,
}

#[wasm_bindgen]
impl SynclineFile {
    #[wasm_bindgen(constructor)]
    pub fn new(url: String, callback: Function) -> Result<SynclineFile, JsValue> {
        // Set panic hook for better debugging
        console_error_panic_hook::set_once();

        let doc = Doc::new();
        let client = SynclineFile {
            url,
            doc,
            ws: None,
            _closures: Rc::new(RefCell::new(Vec::new())),
            _sub: None,
            callback,
            is_synced: Rc::new(RefCell::new(false)),
        };
        Ok(client)
    }

    pub fn connect(&mut self) -> Result<(), JsValue> {
        let ws = WebSocket::new(&self.url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let doc = self.doc.clone();
        let ws_clone = ws.clone();
        let closures = self._closures.clone();
        let callback = self.callback.clone();

        // 1. ON OPEN

        let onopen = Closure::wrap(Box::new(move |_| {
            // Send Sync Step 1: State Vector
            let sv = doc.transact().state_vector().encode_v1();
            let mut msg = vec![MSG_SYNC_STEP_1];
            msg.extend(sv);

            let array = Uint8Array::from(&msg[..]);
            if let Err(e) = ws_clone.send_with_array_buffer_view(&array) {
                web_sys::console::error_1(&e);
            }
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        closures.borrow_mut().push(onopen);

        // 2. ON MESSAGE
        let doc_on_msg = self.doc.clone();
        let callback_clone = callback.clone();
        let is_synced_msg = self.is_synced.clone();

        let onmessage = Closure::wrap(Box::new(move |val: JsValue| {
            let e = val.unchecked_into::<MessageEvent>();
            if let Ok(ab) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = Uint8Array::new(&ab);
                let vec = array.to_vec();

                if vec.is_empty() {
                    return;
                }
                let msg_type = vec[0];
                let payload = &vec[1..];

                match msg_type {
                    MSG_SYNC_STEP_2 | MSG_UPDATE => {
                        if let Ok(u) = Update::decode_v1(payload) {
                            // Set flag to avoid echoing back
                            *is_synced_msg.borrow_mut() = true;
                            {
                                let mut txn = doc_on_msg.transact_mut();
                                txn.apply_update(u);
                            }
                            *is_synced_msg.borrow_mut() = false;

                            // Notify JS
                            let this = JsValue::NULL;
                            match callback_clone.call0(&this) {
                                Ok(_) => {}
                                Err(e) => web_sys::console::error_1(&e),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        closures.borrow_mut().push(onmessage);

        // 3. ON ERROR
        let onerror = Closure::wrap(Box::new(move |val: JsValue| {
            web_sys::console::error_1(&val);
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        closures.borrow_mut().push(onerror);

        // 4. OBSERVE LOCAL CHANGES
        let ws_clone_3 = ws.clone();
        let is_synced_obs = self.is_synced.clone();
        // observe_update_v1 returns Result<Subscription, _>
        // We use unwrap or map_err because we are in a Result returning function
        let sub = self
            .doc
            .observe_update_v1(move |_, event| {
                // Check flag
                if *is_synced_obs.borrow() {
                    return;
                }

                let update = event.update.clone();
                let mut msg = vec![MSG_UPDATE];
                msg.extend(update);
                let array = Uint8Array::from(&msg[..]);
                if let Err(e) = ws_clone_3.send_with_array_buffer_view(&array) {
                    web_sys::console::error_1(&e);
                }
            })
            .map_err(|_| JsValue::from_str("Failed to subscribe"))?;

        self.ws = Some(ws);
        self._sub = Some(sub);

        Ok(())
    }

    pub fn get_text(&self) -> String {
        let text = self.doc.get_or_insert_text("content");
        let txn = self.doc.transact();
        text.get_string(&txn)
    }

    pub fn update(&mut self, new_content: String) {
        let text = self.doc.get_or_insert_text("content");

        // We need to release txn before sending (observer is async/callback but yrs might lock)
        let mut txn = self.doc.transact_mut();
        let current_content = text.get_string(&txn);

        if current_content == new_content {
            return;
        }

        let diffs = diff::chars(&current_content, &new_content);
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

    pub fn set_text(&mut self, content: String) {
        let text = self.doc.get_or_insert_text("content");
        let mut txn = self.doc.transact_mut();
        let len = text.len(&txn);
        if len > 0 {
            text.remove_range(&mut txn, 0, len);
        }
        text.insert(&mut txn, 0, &content);
    }
}
