use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};
use tracing::{error, info};
use url::Url;

use crate::protocol::Message;

pub struct SynclineClient {
    pub url: Url,
    ws_tx: Option<mpsc::Sender<Message>>,
}

impl SynclineClient {
    pub fn new(url: &str) -> Result<Self> {
        let url = Url::parse(url)?;
        Ok(Self { url, ws_tx: None })
    }

    pub async fn connect(
        &mut self,
        app_tx: mpsc::Sender<Message>,
    ) -> Result<mpsc::Sender<Message>> {
        let (ws_stream, _) = connect_async(self.url.as_str()).await?;
        info!("Successfully connected to server at {}", self.url);

        let (mut write, mut read) = ws_stream.split();

        // Channel for app to send messages to the server (over ws)
        let (tx, mut rx) = mpsc::channel::<Message>(1000);

        self.ws_tx = Some(tx.clone());

        let _write_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let encoded = msg.encode();
                if let Err(e) = write
                    .send(WsMessage::Binary(bytes::Bytes::from(encoded)))
                    .await
                {
                    error!("Failed to write to WebSocket: {:?}", e);
                    break;
                }
            }
        });

        let _read_task = tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(WsMessage::Binary(bin)) => {
                        tracing::debug!("Received WS Binary of len: {}", bin.len());
                        match Message::decode(&bin) {
                            Ok(parsed_msg) => {
                                tracing::debug!("Decoded msg of type: {:?}", parsed_msg.msg_type);
                                if let Err(e) = app_tx.send(parsed_msg).await {
                                    error!("Failed to route message to app: {:?}", e);
                                    break;
                                }
                            }
                            Err(e) => {
                                error!("Failed to decode incoming binary message: {:?}", e);
                            }
                        }
                    }
                    Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => {
                        // handled automatically usually, but could be logged
                    }
                    Ok(other) => {
                        error!("Received unexpected websocket message type: {:?}", other);
                    }
                    Err(e) => {
                        error!("WebSocket read error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(tx)
    }
}
