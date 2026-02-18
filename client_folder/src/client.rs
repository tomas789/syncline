use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, StateVector, Subscription, Transact, Update};

use syncline::protocol::{decode_message, encode_message, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE};

struct SendSubscription(#[allow(dead_code)] Subscription);
unsafe impl Send for SendSubscription {}
unsafe impl Sync for SendSubscription {}

struct DocState {
    doc: Doc,
    suppress: Arc<AtomicBool>,
    _sub: Arc<SendSubscription>,
}

#[derive(Clone)]
pub struct Client {
    tx: mpsc::Sender<Vec<u8>>,
    docs: Arc<RwLock<HashMap<String, DocState>>>,
}

impl Client {
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
        let docs: Arc<RwLock<HashMap<String, DocState>>> = Arc::new(RwLock::new(HashMap::new()));
        let docs_clone = docs.clone();
        let url = Url::parse(url)?;

        tokio::spawn(async move {
            loop {
                match connect_async(url.clone()).await {
                    Ok((ws_stream, _)) => {
                        log::info!("Connected to {}", url);
                        let (mut write, mut read) = ws_stream.split();

                        // Send Sync Step 1 AND current state for all docs
                        let docs = docs_clone.read().await;
                        let messages: Vec<Vec<u8>> = docs.iter().map(|(doc_id, state)| {
                            // SYNC_STEP_1 to request remote state
                            let sv = state.doc.transact().state_vector().encode_v1();
                            encode_message(MSG_SYNC_STEP_1, doc_id, &sv)
                        }).collect();
                        drop(docs);
                        
                        for msg in messages {
                            if let Err(e) = write.send(Message::Binary(msg)).await {
                                log::error!("Failed to send handshake: {}", e);
                            }
                        }
                        
                        // Send current state as UPDATE for all docs
                        let docs = docs_clone.read().await;
                        let updates: Vec<(String, Vec<u8>)> = docs.iter().filter_map(|(doc_id, state)| {
                            let txn = state.doc.transact();
                            let update = txn.encode_state_as_update_v1(&StateVector::default());
                            if update.is_empty() {
                                None
                            } else {
                                Some((doc_id.clone(), update))
                            }
                        }).collect();
                        drop(docs);
                        
                        for (doc_id, update) in updates {
                            let msg = encode_message(MSG_UPDATE, &doc_id, &update);
                            if let Err(e) = write.send(Message::Binary(msg)).await {
                                log::error!("Failed to send initial state for {}: {}", doc_id, e);
                            }
                        }

                        // Main loop
                        loop {
                            tokio::select! {
                                msg = rx.recv() => {
                                    match msg {
                                        Some(data) => {
                                            if write.send(Message::Binary(data)).await.is_err() {
                                                break;
                                            }
                                        }
                                        None => return,
                                    }
                                }
                                income = read.next() => {
                                    match income {
                                        Some(Ok(Message::Binary(data))) => {
                                            if let Some((msg_type, doc_id, payload)) = decode_message(&data) {
                                                let docs = docs_clone.read().await;
                                                if let Some(state) = docs.get(doc_id) {
                                                    match msg_type {
                                                        MSG_SYNC_STEP_2 | MSG_UPDATE => {
                                                            state.suppress.store(true, Ordering::SeqCst);
                                                            if let Ok(u) = Update::decode_v1(payload) {
                                                                let mut txn = state.doc.transact_mut();
                                                                txn.apply_update(u);
                                                            }
                                                            state.suppress.store(false, Ordering::SeqCst);
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                        }
                                        Some(Ok(Message::Close(_))) => break,
                                        Some(Err(_)) => break,
                                        None => break,
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::debug!("Connection failed: {}", e);
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });

        Ok(Self { tx, docs })
    }

    pub async fn add_doc(&self, doc_id: String, doc: Doc) -> anyhow::Result<()> {
        let suppress = Arc::new(AtomicBool::new(false));
        let tx_clone = self.tx.clone();
        let suppress_clone = suppress.clone();
        let doc_id_clone = doc_id.clone();

        let sub = doc.observe_update_v1(move |_txn, event| {
            if suppress_clone.load(Ordering::SeqCst) {
                return;
            }
            let msg = encode_message(MSG_UPDATE, &doc_id_clone, &event.update);
            let _ = tx_clone.try_send(msg);
        });

        let sub = Arc::new(SendSubscription(
            sub.map_err(|_| anyhow::anyhow!("Failed to observe doc"))?,
        ));

        // Register doc BEFORE sending Sync Step 1 so we can receive the response
        self.docs.write().await.insert(
            doc_id.clone(),
            DocState {
                doc: doc.clone(),
                suppress,
                _sub: sub,
            },
        );

        // Send Sync Step 1 for this doc
        let sv = doc.transact().state_vector().encode_v1();
        let msg = encode_message(MSG_SYNC_STEP_1, &doc_id, &sv);
        let _ = self.tx.send(msg).await;

        Ok(())
    }

    pub async fn remove_doc(&self, doc_id: &str) {
        self.docs.write().await.remove(doc_id);
    }

    pub async fn send_update(&self, doc_id: &str, update: Vec<u8>) -> anyhow::Result<()> {
        let msg = encode_message(MSG_UPDATE, doc_id, &update);
        let _ = self.tx.send(msg).await;
        Ok(())
    }

    pub fn get_doc(&self, doc_id: &str) -> Option<Doc> {
        let docs = self.docs.try_read().ok()?;
        docs.get(doc_id).map(|s| s.doc.clone())
    }
}