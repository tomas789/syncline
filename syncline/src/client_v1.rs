//! Minimal v1 client — handshake + manifest sync, no local-change
//! pipeline yet. Enough to get a client process to connect, negotiate
//! the v1 protocol with the server, and keep its local manifest in
//! sync with the server's broadcast stream.
//!
//! Scope of this module (Phase 3.3a):
//!   - run_client entry point (called from main.rs for `syncline sync`)
//!   - v1 MSG_VERSION handshake
//!   - idempotent on-disk v1 vault layout via v1::disk
//!   - initial MANIFEST_SYNC STEP_1, apply returned STEP_2
//!   - apply incoming MANIFEST_UPDATE frames, persist manifest to disk
//!   - reconnect loop with exponential-ish backoff
//!
//! Out of scope here, added in follow-up commits on release/v1:
//!   - polling / filesystem watcher → local change detection
//!   - content subdoc sync (per-text-node)
//!   - blob upload / download
//!   - reconcile projection → filesystem
//!   - conflict-copy path suffixing

use crate::protocol::{
    MANIFEST_DOC_ID, MSG_MANIFEST_SYNC, MSG_VERSION, V1_PROTOCOL_MAJOR,
    V1_PROTOCOL_MINOR, decode_message, encode_message,
};
use crate::v1::disk::{migrate_vault_on_disk, read_or_create_actor_id};
use crate::v1::ids::{ActorId, Lamport};
use crate::v1::manifest::Manifest;
use crate::v1::sync::{
    decode_version_handshake, encode_version_handshake, handle_manifest_payload,
    manifest_step1_payload,
};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};
use tracing::{debug, error, info, warn};
use yrs::{ReadTxn, StateVector, Transact};

const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_CAP_MS: u64 = 30_000;

/// Entry point for `syncline sync`. Blocks for the lifetime of the
/// client, reconnecting on transport errors.
pub async fn run_client(
    folder: PathBuf,
    url: String,
    _name: Option<String>,
) -> Result<()> {
    banner(&folder, &url);

    // Ensure v1 layout on disk. Idempotent — a v1 vault is a no-op,
    // a v0 vault gets migrated.
    let report = tokio::task::spawn_blocking({
        let folder = folder.clone();
        move || migrate_vault_on_disk(&folder)
    })
    .await??;
    if !report.already_migrated {
        info!(
            "Migrated local vault: {} text, {} binary, {} directories",
            report.text_files, report.binary_files, report.directories
        );
        for w in &report.warnings {
            warn!("migration warning: {}", w);
        }
    }

    let syncline_dir = folder.join(".syncline");
    let actor = read_or_create_actor_id(&syncline_dir)?;

    // Load (or create) the manifest once; it's the source of truth
    // for the whole run. The reconnect loop shares this manifest so
    // state persists across transport hiccups.
    let mut manifest = load_manifest(&syncline_dir, actor)?;

    let mut attempt: u32 = 0;
    loop {
        match run_session(&url, &mut manifest, &syncline_dir).await {
            Ok(()) => {
                // Graceful close (server shutdown). Retry after base
                // backoff; this is not a hard error.
                warn!("session ended cleanly; reconnecting in {} ms", RECONNECT_BASE_MS);
                attempt = 0;
                tokio::time::sleep(Duration::from_millis(RECONNECT_BASE_MS)).await;
            }
            Err(e) => {
                attempt = attempt.saturating_add(1);
                let delay = backoff_ms(attempt);
                error!(attempt, delay_ms = delay, "session failed: {e:?}");
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
    }
}

/// Single connect + sync session. Returns Ok when the server closes
/// cleanly (or our read half drops), Err on any protocol or transport
/// failure.
async fn run_session(
    url: &str,
    manifest: &mut Manifest,
    syncline_dir: &Path,
) -> Result<()> {
    info!("connecting to {}", url);
    let (ws, _) = connect_async(url).await.context("ws connect")?;
    let (mut write, mut read) = ws.split();

    // --- Version handshake (step 1) -----------------------------------------
    let hs = encode_message(
        MSG_VERSION,
        MANIFEST_DOC_ID,
        &encode_version_handshake(),
    );
    write
        .send(WsMessage::Binary(hs.into()))
        .await
        .context("send version handshake")?;

    // Server must echo its version back. If it closes the socket
    // instead, that's a protocol mismatch on the other end.
    let first = match read.next().await {
        Some(Ok(WsMessage::Binary(b))) => b,
        Some(Ok(WsMessage::Close(_))) | None => {
            anyhow::bail!("server closed during handshake — likely non-v1 server");
        }
        Some(Ok(other)) => anyhow::bail!("unexpected frame during handshake: {other:?}"),
        Some(Err(e)) => anyhow::bail!("transport error during handshake: {e}"),
    };
    let (t, d, payload) = decode_message(&first)
        .ok_or_else(|| anyhow::anyhow!("malformed handshake reply frame"))?;
    if t != MSG_VERSION || d != MANIFEST_DOC_ID {
        anyhow::bail!("server did not reply with MSG_VERSION (got msg_type {t:#x})");
    }
    let Some((major, minor)) = decode_version_handshake(payload) else {
        anyhow::bail!("server handshake payload is malformed");
    };
    if major != V1_PROTOCOL_MAJOR {
        anyhow::bail!(
            "server protocol {}.{} incompatible with client {}.{}",
            major,
            minor,
            V1_PROTOCOL_MAJOR,
            V1_PROTOCOL_MINOR
        );
    }
    info!("v1 handshake OK (server {}.{})", major, minor);

    // --- Initial manifest sync (step 2) -------------------------------------
    let step1 = manifest_step1_payload(manifest);
    let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &step1);
    write
        .send(WsMessage::Binary(frame.into()))
        .await
        .context("send manifest step1")?;

    // --- Read loop ----------------------------------------------------------
    while let Some(msg) = read.next().await {
        let data = match msg {
            Ok(WsMessage::Binary(b)) => b,
            Ok(WsMessage::Close(_)) => {
                info!("server closed connection");
                return Ok(());
            }
            Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => continue,
            Ok(other) => {
                debug!("ignoring {:?} frame", other);
                continue;
            }
            Err(e) => anyhow::bail!("ws read error: {e}"),
        };
        let Some((msg_type, doc_id, payload)) = decode_message(&data) else {
            warn!("dropping malformed frame");
            continue;
        };
        if doc_id != MANIFEST_DOC_ID {
            // Content subdocs + blobs land in this branch and are
            // handled in the later 3.3b / 3.3c commits. Drop for now
            // so we don't leak memory holding onto untranslated bytes.
            debug!("dropping non-manifest frame for doc_id={}", doc_id);
            continue;
        }
        match msg_type {
            MSG_MANIFEST_SYNC => {
                match handle_manifest_payload(manifest, payload) {
                    Ok(reply) => {
                        if let Some(reply_payload) = reply {
                            let frame = encode_message(
                                MSG_MANIFEST_SYNC,
                                MANIFEST_DOC_ID,
                                &reply_payload,
                            );
                            if let Err(e) = write
                                .send(WsMessage::Binary(frame.into()))
                                .await
                            {
                                anyhow::bail!("ws write during manifest reply: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("applying manifest payload: {e}");
                        continue;
                    }
                }
                // Persist after every successful apply so a crashed
                // client resumes with up-to-date state.
                if let Err(e) = save_manifest(syncline_dir, manifest) {
                    error!("persisting manifest: {e}");
                }
            }
            other => {
                debug!("ignoring manifest doc frame msg_type={:#x}", other);
            }
        }
    }
    // read stream ended with no close frame — treat as transport loss
    anyhow::bail!("ws read stream ended without a close frame");
}

// ---------------------------------------------------------------------------
// On-disk manifest IO
// ---------------------------------------------------------------------------

fn load_manifest(syncline_dir: &Path, actor: ActorId) -> Result<Manifest> {
    let path = syncline_dir.join("manifest.bin");
    if !path.exists() {
        return Ok(Manifest::new(actor));
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    Manifest::from_update(actor, Lamport::ZERO, &bytes)
        .with_context(|| format!("decoding manifest at {}", path.display()))
}

fn save_manifest(syncline_dir: &Path, manifest: &Manifest) -> Result<()> {
    fs::create_dir_all(syncline_dir).context("mkdir .syncline")?;
    let bytes = {
        let txn = manifest.doc().transact();
        txn.encode_state_as_update_v1(&StateVector::default())
    };
    let path = syncline_dir.join("manifest.bin");
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp).context("create manifest tmp")?;
        f.write_all(&bytes).context("write manifest tmp")?;
        f.sync_all().context("fsync manifest tmp")?;
    }
    fs::rename(&tmp, &path).context("rename manifest tmp")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Backoff + pretty-print
// ---------------------------------------------------------------------------

fn backoff_ms(attempt: u32) -> u64 {
    let shifted = RECONNECT_BASE_MS.saturating_mul(1u64 << attempt.min(6));
    shifted.min(RECONNECT_CAP_MS)
}

fn banner(folder: &Path, url: &str) {
    use colored::Colorize;
    println!(
        "{}",
        r#"
   _____                  ___
  / ___/__  ______  _____/ (_)___  ___
  \__ \/ / / / __ \/ ___/ / / __ \/ _ \
 ___/ / /_/ / / / / /__/ / / / / /  __/
/____/\__, /_/ /_/\___/_/_/_/ /_/\___/
     /____/
"#
        .cyan()
        .bold()
    );
    println!("  {}\n", "🌟 Syncline v1 client".green());
    info!("{} Folder: {}", "📂".blue(), folder.display());
    info!("{} Server URL: {}", "🌐".cyan(), url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_reconnect_cap() {
        for i in 0..20u32 {
            assert!(backoff_ms(i) <= RECONNECT_CAP_MS);
        }
        assert_eq!(backoff_ms(0), RECONNECT_BASE_MS);
        assert!(backoff_ms(10) == RECONNECT_CAP_MS);
    }

    #[tokio::test]
    async fn manifest_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let syncline_dir = dir.path().join(".syncline");
        fs::create_dir_all(&syncline_dir).unwrap();

        let actor = ActorId::new();
        let mut m = Manifest::new(actor);
        crate::v1::ops::create_text(&mut m, "hello.md", 5).unwrap();

        save_manifest(&syncline_dir, &m).unwrap();

        let loaded = load_manifest(&syncline_dir, actor).unwrap();
        assert_eq!(loaded.live_entries().len(), 1);
        assert_eq!(loaded.live_entries()[0].name, "hello.md");
    }

    #[test]
    fn missing_manifest_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let syncline_dir = dir.path().join(".syncline");
        let m = load_manifest(&syncline_dir, ActorId::new()).unwrap();
        assert!(m.live_entries().is_empty());
    }
}
