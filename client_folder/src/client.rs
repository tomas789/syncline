use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, Transact, Update};

// Message types
use syncline::protocol::{MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE};

/// Wraps yrs::Subscription to be Send + Sync (needed for cross-thread usage)
struct SendSubscription(#[allow(dead_code)] yrs::Subscription);
unsafe impl Send for SendSubscription {}
unsafe impl Sync for SendSubscription {}

#[derive(Clone)]
pub struct Client {
    doc: Doc,
    tx: mpsc::Sender<Vec<u8>>,
    /// Whether to suppress auto-sending (to avoid echo loops).
    /// Kept alive here so the Arc clones in spawned tasks remain valid.
    #[allow(dead_code)]
    suppress_send: Arc<AtomicBool>,
    /// Keep the observe subscription alive
    _sub: Arc<SendSubscription>,
}

impl Client {
    pub async fn new(url: &str, doc: Doc) -> anyhow::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
        let doc_clone = doc.clone();
        let url = Url::parse(url)?; // Validate URL once
        let suppress_send = Arc::new(AtomicBool::new(false));
        let suppress_clone = suppress_send.clone();

        tokio::spawn(async move {
            loop {
                // Exponential backoff could be added here
                match connect_async(url.clone()).await {
                    Ok((ws_stream, _)) => {
                        log::info!("Connected to {}", url);
                        let (mut write, mut read) = ws_stream.split();

                        // Sync Step 1: Send local State Vector
                        let sv = doc_clone.transact().state_vector().encode_v1();
                        let mut msg = vec![MSG_SYNC_STEP_1];
                        msg.extend(sv);
                        if let Err(e) = write.send(Message::Binary(msg)).await {
                            log::error!("Failed to send handshake: {}", e);
                            continue;
                        }

                        // Main loop
                        loop {
                            tokio::select! {
                                msg = rx.recv() => {
                                    match msg {
                                        Some(data) => {
                                            if write.send(Message::Binary(data)).await.is_err() {
                                                break; // Connection failed
                                            }
                                        }
                                        None => return, // Client dropped, exit task
                                    }
                                }
                                income = read.next() => {
                                    match income {
                                        Some(Ok(Message::Binary(data))) => {
                                            if data.is_empty() { continue; }
                                            let msg_type = data[0];
                                            let payload = &data[1..];

                                            match msg_type {
                                                MSG_SYNC_STEP_2 | MSG_UPDATE => {
                                                    suppress_clone.store(true, Ordering::SeqCst);
                                                    if let Ok(u) = Update::decode_v1(payload) {
                                                        let mut txn = doc_clone.transact_mut();
                                                        txn.apply_update(u);
                                                    }
                                                    suppress_clone.store(false, Ordering::SeqCst);
                                                }
                                                _ => {}
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
                        // Only log if not just "connection refused" spam
                        log::debug!("Connection failed: {}", e);
                    }
                }

                // Wait before reconnecting
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });

        // Auto-send: observe all doc updates and forward them
        let tx_auto = tx.clone();
        let suppress_auto = suppress_send.clone();
        let sub = doc.observe_update_v1(move |_txn, event| {
            if suppress_auto.load(Ordering::SeqCst) {
                return; // Don't re-send remote updates
            }
            let update = event.update.clone();
            let mut msg = vec![MSG_UPDATE];
            msg.extend(update);
            let _ = tx_auto.try_send(msg);
        });

        Ok(Self {
            doc,
            tx,
            suppress_send,
            _sub: Arc::new(SendSubscription(
                sub.expect("Failed to observe doc updates"),
            )),
        })
    }

    pub async fn send_update(&self, update: Vec<u8>) -> anyhow::Result<()> {
        let mut msg = vec![MSG_UPDATE];
        msg.extend(update);
        let _ = self.tx.send(msg).await; // In new loop, this queues in channel
        Ok(())
    }

    pub fn get_doc(&self) -> &Doc {
        &self.doc
    }
}
