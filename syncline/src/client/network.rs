use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::sync::Once;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};
use tracing::{error, info};
use url::Url;

use crate::client::protocol::Message;

// rustls 0.23 has no default CryptoProvider — pick one explicitly so that
// `wss://` connections work out of the box. Idempotent and cheap to call.
fn ensure_rustls_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub struct SynclineClient {
    pub url: Url,
    ws_tx: Option<mpsc::Sender<Message>>,
    write_task: Option<tokio::task::JoinHandle<()>>,
    read_task: Option<tokio::task::JoinHandle<()>>,
}

impl SynclineClient {
    pub fn new(url: &str) -> Result<Self> {
        let url = Url::parse(url)?;
        Ok(Self {
            url,
            ws_tx: None,
            write_task: None,
            read_task: None,
        })
    }

    pub async fn connect(
        &mut self,
        app_tx: mpsc::Sender<Message>,
    ) -> Result<mpsc::Sender<Message>> {
        if let Some(task) = self.write_task.take() {
            task.abort();
        }
        if let Some(task) = self.read_task.take() {
            task.abort();
        }

        ensure_rustls_provider();
        let (ws_stream, _) = connect_async(self.url.as_str()).await?;
        info!("Successfully connected to server at {}", self.url);

        let (mut write, mut read) = ws_stream.split();

        // Channel for app to send messages to the server (over ws)
        let (tx, mut rx) = mpsc::channel::<Message>(1000);

        self.ws_tx = Some(tx.clone());

        self.write_task = Some(tokio::spawn(async move {
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
        }));

        self.read_task = Some(tokio::spawn(async move {
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
        }));

        Ok(tx)
    }
}

impl Drop for SynclineClient {
    fn drop(&mut self) {
        if let Some(task) = self.write_task.take() {
            task.abort();
        }
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    async fn spawn_test_server() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let active_connections = Arc::new(AtomicUsize::new(0));
        let counter_clone = active_connections.clone();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let counter = counter_clone.clone();
                tokio::spawn(async move {
                    if let Ok(mut ws) = accept_async(stream).await {
                        counter.fetch_add(1, Ordering::SeqCst);
                        // Keep connection alive until closed by client
                        while ws.next().await.is_some() {}
                        counter.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });

        (port, active_connections)
    }

    #[tokio::test]
    async fn test_task_leak_on_reconnect() {
        let (port, active_connections) = spawn_test_server().await;
        let url = format!("ws://127.0.0.1:{}", port);

        let mut client = SynclineClient::new(&url).unwrap();
        let (app_tx, _) = mpsc::channel(10);

        client.connect(app_tx.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            active_connections.load(Ordering::SeqCst),
            1,
            "Should have 1 active connection"
        );

        // Reconnect. If tasks are leaked, the old connection keeps alive.
        client.connect(app_tx).await.unwrap();

        // Wait up to 2 seconds for old connection to be properly terminated
        let mut converged = false;
        for _ in 0..25 {
            if active_connections.load(Ordering::SeqCst) == 1 {
                converged = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert!(
            converged,
            "Should STILL have 1 active connection after reconnect, but found {}",
            active_connections.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn test_drop_disconnects() {
        let (port, active_connections) = spawn_test_server().await;
        let url = format!("ws://127.0.0.1:{}", port);

        let client = {
            let mut client = SynclineClient::new(&url).unwrap();
            let (app_tx, _) = mpsc::channel(10);
            client.connect(app_tx).await.unwrap();

            // Wait for connection to establish
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(active_connections.load(Ordering::SeqCst), 1);
            client
        };

        // Client is dropped here
        drop(client);

        // Wait for connection to drop
        let mut converged = false;
        for _ in 0..25 {
            if active_connections.load(Ordering::SeqCst) == 0 {
                converged = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert!(
            converged,
            "Should have 0 active connections after drop, but found {}",
            active_connections.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn test_invalid_url() {
        let result = SynclineClient::new("invalid_url");
        assert!(result.is_err());
    }

    // Regression test for #54: released binaries failed against `wss://` URLs
    // with "TLS support not compiled in" because tokio-tungstenite's TLS
    // features were disabled. We accept a TCP connection on a local port
    // (so resolution + TCP connect succeed) and then attempt the WSS
    // upgrade. Without TLS compiled in, this fails with the "not compiled"
    // error; with rustls enabled, it fails with a real TLS handshake error
    // (because our dummy listener doesn't actually speak TLS).
    #[tokio::test]
    async fn test_wss_url_does_not_fail_with_tls_not_compiled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept loop — just keep the TCP connection open long enough for
        // the TLS handshake attempt to fail in the way we expect.
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    drop(stream);
                });
            }
        });

        let url = format!("wss://localhost:{port}/sync");
        let mut client = SynclineClient::new(&url).unwrap();
        let (app_tx, _rx) = mpsc::channel(10);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            client.connect(app_tx),
        )
        .await
        .expect("connect_async should not hang");

        let err = result.expect_err("wss to non-TLS endpoint must fail");
        let msg = format!("{:#}", err);
        assert!(
            !msg.contains("TLS support not compiled in"),
            "wss:// must not fail with 'TLS support not compiled in' — got: {msg}"
        );
    }
}
