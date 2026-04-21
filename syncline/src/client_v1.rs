//! Minimal v1 client — handshake + manifest sync, no local-change
//! pipeline yet. Enough to get a client process to connect, negotiate
//! the v1 protocol with the server, and keep its local manifest in
//! sync with the server's broadcast stream.
//!
//! Scope of this module (Phase 3.3a + 3.3b inbound):
//!   - run_client entry point (called from main.rs for `syncline sync`)
//!   - v1 MSG_VERSION handshake
//!   - idempotent on-disk v1 vault layout via v1::disk
//!   - initial MANIFEST_SYNC STEP_1, apply returned STEP_2
//!   - apply incoming MANIFEST_UPDATE frames, persist manifest to disk
//!   - conservative projection → filesystem reconcile (create-only):
//!     mkdir missing dirs, touch missing text files as empty placeholders,
//!     skip binaries (blob fetch lands in 3.3c), never delete anything
//!   - **3.3b inbound:** per-text-node content subdoc sync. For every
//!     live Text projection entry we have not yet subscribed to in this
//!     session, send `MSG_SYNC_STEP_1` on `content:<hex>`. Incoming
//!     `MSG_SYNC_STEP_2` / `MSG_UPDATE` frames are applied to the local
//!     content subdoc, persisted to `.syncline/content/<id>.bin`, then
//!     the resulting Y.Text body is written through to the projected
//!     file path on disk.
//!   - reconnect loop with exponential-ish backoff
//!
//! Out of scope here, added in follow-up commits on release/v1:
//!   - polling / filesystem watcher → local change detection (3.3b.2)
//!   - blob upload / download (3.3c)
//!   - deletion of files that have no live manifest entry
//!   - conflict-copy path suffixing

use crate::protocol::{
    MANIFEST_DOC_ID, MAX_BLOB_SIZE, MSG_BLOB_REQUEST, MSG_BLOB_UPDATE, MSG_MANIFEST_SYNC,
    MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE, MSG_VERSION, V1_PROTOCOL_MAJOR,
    V1_PROTOCOL_MINOR, decode_message, encode_message,
};
use crate::v1::blob_store::{BlobStore, hash_hex};
use crate::v1::disk::{migrate_vault_on_disk, read_or_create_actor_id};
use crate::v1::ids::{ActorId, Lamport, NodeId};
use crate::v1::manifest::{Manifest, NodeKind};
use crate::v1::projection::{Projection, project};
use crate::v1::sync::{
    decode_version_handshake, encode_manifest_update, encode_version_handshake,
    handle_manifest_payload, manifest_step1_payload,
};
use anyhow::{Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message as WsMessage,
};
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;

const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_CAP_MS: u64 = 30_000;
const SCAN_INTERVAL: Duration = Duration::from_secs(5);
const TEXT_EXTS: &[&str] = &["md", "txt"];

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
    let mut content = ContentStore::new(syncline_dir.join("content"));
    let blobs = BlobStore::new(syncline_dir.join("blobs"));

    let mut attempt: u32 = 0;
    loop {
        match run_session(&url, &mut manifest, &mut content, &blobs, &folder, &syncline_dir).await {
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
    content: &mut ContentStore,
    blobs: &BlobStore,
    folder: &Path,
    syncline_dir: &Path,
) -> Result<()> {
    info!("connecting to {}", url);
    let (ws, _) = connect_async(url).await.context("ws connect")?;
    let (mut write, mut read) = ws.split();

    // Tracks text-node subdocs for which we've sent STEP_1 this session.
    // Prevents spamming the server after every manifest apply while still
    // letting us pick up newly-created entries immediately.
    let mut content_subscribed: HashSet<NodeId> = HashSet::new();
    // Tracks blob hashes for which we've already sent MSG_BLOB_REQUEST
    // this session. Cleared on reconnect.
    let mut requested_blobs: HashSet<String> = HashSet::new();

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

    // --- Read loop + polling scanner ---------------------------------------
    let mut scan_timer = interval(SCAN_INTERVAL);
    scan_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // `interval` emits its first tick immediately; consume it so our
    // first *scheduled* scan is SCAN_INTERVAL from now. The explicit
    // initial scan below covers startup.
    scan_timer.tick().await;

    // Initial scan: push local additions/edits to the server as soon as
    // the session is up. Subsequent scans run on the 5 s timer.
    if let Err(e) = scan_once(
        folder,
        syncline_dir,
        manifest,
        content,
        blobs,
        &mut write,
        &mut content_subscribed,
    )
    .await
    {
        warn!("initial scan failed: {e:?}");
    }

    // Cover the case where a previous session left behind a manifest
    // referring to blobs we never fully fetched — or already have locally.
    // Materialise whatever we can, and request anything still missing.
    if let Err(e) = reconcile_projection_to_disk(folder, manifest, blobs) {
        warn!("initial reconcile failed: {e:?}");
    }
    if let Err(e) =
        request_missing_blobs(&mut write, manifest, blobs, &mut requested_blobs).await
    {
        warn!("initial blob requests failed: {e:?}");
    }

    loop {
        tokio::select! {
            biased;
            msg_opt = read.next() => {
                let Some(msg) = msg_opt else {
                    anyhow::bail!("ws read stream ended without a close frame");
                };
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
                if msg_type == MSG_BLOB_UPDATE {
                    if let Err(e) = handle_inbound_blob(doc_id, payload, blobs) {
                        warn!("inbound blob rejected: {e:?}");
                        continue;
                    }
                    if let Err(e) = reconcile_projection_to_disk(folder, manifest, blobs) {
                        error!("reconcile after blob arrival: {e}");
                    }
                    continue;
                }
                if doc_id == MANIFEST_DOC_ID {
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
                                            anyhow::bail!(
                                                "ws write during manifest reply: {e}"
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("applying manifest payload: {e}");
                                    continue;
                                }
                            }
                            if let Err(e) = save_manifest(syncline_dir, manifest) {
                                error!("persisting manifest: {e}");
                            }
                            if let Err(e) =
                                reconcile_projection_to_disk(folder, manifest, blobs)
                            {
                                error!("reconciling projection: {e}");
                            }
                            if let Err(e) = subscribe_new_text_content(
                                &mut write,
                                manifest,
                                content,
                                &mut content_subscribed,
                            )
                            .await
                            {
                                anyhow::bail!("content STEP_1 broadcast: {e}");
                            }
                            if let Err(e) = request_missing_blobs(
                                &mut write,
                                manifest,
                                blobs,
                                &mut requested_blobs,
                            )
                            .await
                            {
                                anyhow::bail!("blob request broadcast: {e}");
                            }
                        }
                        other => {
                            debug!("ignoring manifest doc frame msg_type={:#x}", other);
                        }
                    }
                    continue;
                }

                if let Some(node_id) = parse_content_doc_id(doc_id) {
                    match msg_type {
                        MSG_SYNC_STEP_2 | MSG_UPDATE => {
                            if let Err(e) = content.apply_update(node_id, payload) {
                                error!("apply content update for {:?}: {e}", node_id);
                                continue;
                            }
                            if let Err(e) = content.persist(node_id) {
                                error!("persist content subdoc for {:?}: {e}", node_id);
                            }
                            if let Err(e) =
                                flush_content_to_disk(folder, manifest, content, node_id)
                            {
                                error!("write content to disk for {:?}: {e}", node_id);
                            }
                        }
                        MSG_SYNC_STEP_1 => {
                            debug!("unexpected STEP_1 for {}", doc_id);
                        }
                        other => {
                            debug!(
                                "ignoring content frame msg_type={:#x} for {}",
                                other, doc_id
                            );
                        }
                    }
                    continue;
                }

                debug!("dropping frame for unknown doc_id={}", doc_id);
            }
            _ = scan_timer.tick() => {
                if let Err(e) = scan_once(
                    folder,
                    syncline_dir,
                    manifest,
                    content,
                    blobs,
                    &mut write,
                    &mut content_subscribed,
                )
                .await
                {
                    warn!("periodic scan failed: {e:?}");
                }
            }
        }
    }
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
    let bytes = {
        let txn = manifest.doc().transact();
        txn.encode_state_as_update_v1(&StateVector::default())
    };
    atomic_write(&syncline_dir.join("manifest.bin"), &bytes)
}

// ---------------------------------------------------------------------------
// Polling scanner (Phase 3.3b.2 outbound)
// ---------------------------------------------------------------------------

/// Walk the vault once, detect local-only text files and modifications,
/// and push them to the server as a single batch.
///
/// Per text file on disk:
///   * not in manifest → `create_text`, seed content subdoc, mark for upload
///   * in manifest and content matches → no-op
///   * in manifest but content differs → apply minimal-edit delta to subdoc
///
/// Binary files are skipped (3.3c). Files missing from disk but present
/// in the manifest are intentionally NOT deleted in this pass (deletion
/// detection requires distinguishing "gone" from "not yet written", and
/// the minimum viable client should not accidentally propagate apparent
/// deletions triggered by transient I/O).
async fn scan_once(
    folder: &Path,
    syncline_dir: &Path,
    manifest: &mut Manifest,
    content: &mut ContentStore,
    blobs: &BlobStore,
    write: &mut WsSink,
    subscribed: &mut HashSet<NodeId>,
) -> Result<()> {
    let pre_sv = manifest.doc().transact().state_vector();

    // Snapshot projection once for path lookups. The loop may grow the
    // manifest, but we rely on `create_text` to detect duplicates and
    // on the caller running scan_once single-threaded.
    let proj = project(manifest);

    let mut pending_content: Vec<(NodeId, Vec<u8>)> = Vec::new();
    // Binary uploads batched until after the walk. (hash_hex, bytes).
    let mut pending_blobs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut new_files = 0usize;
    let mut modified_files = 0usize;
    let mut new_binary = 0usize;
    let mut modified_binary = 0usize;

    for dent in WalkDir::new(folder)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.') && name != ".syncline"
        })
        .filter_map(|e| e.ok())
    {
        if !dent.file_type().is_file() {
            continue;
        }
        let abs = dent.path();
        let Ok(rel) = abs.strip_prefix(folder) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_unsafe_relative_path(&rel_str) {
            continue;
        }
        let ext = rel
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !TEXT_EXTS.contains(&ext.as_str()) {
            // Binary path: read bytes, hash, CAS-stash locally, and
            // create/update the manifest entry. Actual upload is batched
            // and sent at the end of the walk.
            let meta = match fs::metadata(abs) {
                Ok(m) => m,
                Err(e) => {
                    debug!("skip unreadable metadata {}: {}", rel_str, e);
                    continue;
                }
            };
            if meta.len() as usize > MAX_BLOB_SIZE {
                warn!(
                    "skipping {} ({} bytes > MAX_BLOB_SIZE {})",
                    rel_str, meta.len(), MAX_BLOB_SIZE
                );
                continue;
            }
            let bytes = match fs::read(abs) {
                Ok(b) => b,
                Err(e) => {
                    debug!("skip unreadable {}: {}", rel_str, e);
                    continue;
                }
            };
            match process_binary_file(&rel_str, &bytes, &proj, manifest, blobs)? {
                BinaryScanOutcome::Unchanged => {}
                BinaryScanOutcome::Skipped(reason) => {
                    debug!("binary {} skipped: {}", rel_str, reason);
                }
                BinaryScanOutcome::Created { hash } => {
                    new_binary += 1;
                    pending_blobs.push((hash, bytes));
                }
                BinaryScanOutcome::Rehashed { hash } => {
                    modified_binary += 1;
                    pending_blobs.push((hash, bytes));
                }
            }
            continue;
        }

        let body = match fs::read_to_string(abs) {
            Ok(s) => s,
            Err(e) => {
                debug!("skip unreadable/non-UTF8 {}: {}", rel_str, e);
                continue;
            }
        };

        if let Some(existing) = proj.by_path.get(&rel_str) {
            if existing.kind != NodeKind::Text {
                continue;
            }
            if let Some(update) = content.replace_text(existing.id, &body)? {
                content.persist(existing.id)?;
                manifest.record_modify(existing.id);
                pending_content.push((existing.id, update));
                modified_files += 1;
            }
        } else {
            let size = body.len() as u64;
            match crate::v1::ops::create_text(manifest, &rel_str, size) {
                Ok(nid) => {
                    // Seed the brand-new subdoc via `replace_text` (empty → body).
                    if let Some(update) = content.replace_text(nid, &body)? {
                        content.persist(nid)?;
                        pending_content.push((nid, update));
                    }
                    new_files += 1;
                }
                Err(e) => {
                    debug!("create_text({:?}) skipped: {}", rel_str, e);
                }
            }
        }
    }

    // Blobs first, so when the server rebroadcasts the manifest to other
    // peers they can resolve hashes that would otherwise 404.
    for (hash, bytes) in pending_blobs {
        let frame = encode_message(MSG_BLOB_UPDATE, &hash, &bytes);
        write
            .send(WsMessage::Binary(frame.into()))
            .await
            .context("send blob update from scanner")?;
    }

    let post_sv = manifest.doc().transact().state_vector();
    if post_sv != pre_sv {
        let update_bytes = {
            let txn = manifest.doc().transact();
            txn.encode_state_as_update_v1(&pre_sv)
        };
        let payload = encode_manifest_update(&update_bytes);
        let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &payload);
        write
            .send(WsMessage::Binary(frame.into()))
            .await
            .context("send manifest update from scanner")?;
        save_manifest(syncline_dir, manifest)?;
    }

    for (node_id, update) in pending_content {
        let frame = encode_message(MSG_UPDATE, &content_doc_id(node_id), &update);
        write
            .send(WsMessage::Binary(frame.into()))
            .await
            .context("send content update from scanner")?;
    }

    if new_files + modified_files + new_binary + modified_binary > 0 {
        info!(
            new_files,
            modified_files,
            new_binary,
            modified_binary,
            "scanner pushed local changes to server"
        );
    }

    // Newly-created entries get a STEP_1 so we also hear concurrent
    // server-side edits that may already be in flight for that doc id.
    subscribe_new_text_content(write, manifest, content, subscribed).await?;
    Ok(())
}

/// Outcome of scanning a single binary file on disk. Pure enough to
/// unit-test against a fake manifest + temp-dir BlobStore.
#[derive(Debug)]
enum BinaryScanOutcome {
    /// Manifest already has a Binary entry at this path with this hash —
    /// nothing to upload, nothing to record.
    Unchanged,
    /// New path: manifest gained a `Binary` entry; bytes were stashed in
    /// the local blob store and should be uploaded.
    Created { hash: String },
    /// Existing binary path's on-disk content changed: manifest hash
    /// bumped; bytes were stashed locally and should be uploaded.
    Rehashed { hash: String },
    /// A local manifest entry exists at this path but is not `Binary`
    /// (e.g. Text) — or creation fell through with a path-level error.
    /// We refuse to clobber the kind. Caller logs and moves on.
    Skipped(&'static str),
}

/// Core per-binary-file logic, factored out so tests can drive it
/// without a `WsSink`. The caller is responsible for:
///   * reading `bytes` off disk
///   * enforcing `MAX_BLOB_SIZE`
///   * issuing the `MSG_BLOB_UPDATE` frame when we return `Created` /
///     `Rehashed`
///
/// This helper hashes the bytes, CAS-stashes them in `blobs` (idempotent),
/// then reconciles against the current manifest projection:
///   * no existing entry at `rel_path` → `create_binary`, emit `Created`
///   * existing Binary entry with the same hash → `Unchanged`
///   * existing Binary entry with a different hash → `set_blob_hash`,
///     emit `Rehashed`
///   * existing Text/Directory entry at that path → `Skipped("kind")`
fn process_binary_file(
    rel_path: &str,
    bytes: &[u8],
    proj: &Projection,
    manifest: &mut Manifest,
    blobs: &BlobStore,
) -> Result<BinaryScanOutcome> {
    let hash = hash_hex(bytes);
    let size = bytes.len() as u64;

    // Stash locally first — idempotent, and guarantees that if we
    // record the hash in the manifest we actually have the blob to
    // serve to any peer that asks.
    blobs
        .insert_bytes(bytes)
        .with_context(|| format!("stashing blob for {}", rel_path))?;

    match proj.by_path.get(rel_path) {
        None => match crate::v1::ops::create_binary(manifest, rel_path, &hash, size) {
            Ok(_) => Ok(BinaryScanOutcome::Created { hash }),
            Err(e) => {
                debug!("create_binary({:?}) failed: {}", rel_path, e);
                Ok(BinaryScanOutcome::Skipped("create_binary_failed"))
            }
        },
        Some(existing) if existing.kind == NodeKind::Binary => {
            if existing.blob_hash.as_deref() == Some(hash.as_str()) {
                Ok(BinaryScanOutcome::Unchanged)
            } else {
                manifest.set_blob_hash(existing.id, &hash, size);
                Ok(BinaryScanOutcome::Rehashed { hash })
            }
        }
        Some(_) => Ok(BinaryScanOutcome::Skipped("kind_mismatch")),
    }
}

// ---------------------------------------------------------------------------
// Content subdoc store + sync plumbing (Phase 3.3b inbound)
// ---------------------------------------------------------------------------

/// In-memory cache of per-text-node content subdocs, backed by
/// `.syncline/content/<node-id>.bin`. Loads lazily on first touch; each
/// persisted file holds a single Y.Doc with a root `text` Y.Text.
struct ContentStore {
    content_dir: PathBuf,
    docs: HashMap<NodeId, Doc>,
}

impl ContentStore {
    fn new(content_dir: PathBuf) -> Self {
        Self {
            content_dir,
            docs: HashMap::new(),
        }
    }

    fn content_file(content_dir: &Path, node_id: NodeId) -> PathBuf {
        content_dir.join(format!("{}.bin", node_id.to_string_hyphenated()))
    }

    /// Loads the subdoc for `node_id` from disk if present, otherwise
    /// creates a fresh empty one. The root `text` Y.Text is eagerly
    /// materialised so later reads don't race on lazy creation.
    fn ensure_loaded(&mut self, node_id: NodeId) -> Result<()> {
        if self.docs.contains_key(&node_id) {
            return Ok(());
        }
        let doc = Doc::new();
        let path = Self::content_file(&self.content_dir, node_id);
        if path.exists() {
            let bytes = fs::read(&path)
                .with_context(|| format!("read content subdoc {}", path.display()))?;
            let upd = Update::decode_v1(&bytes)
                .with_context(|| format!("decode content subdoc {}", path.display()))?;
            doc.transact_mut().apply_update(upd);
        }
        let _ = doc.get_or_insert_text("text");
        self.docs.insert(node_id, doc);
        Ok(())
    }

    fn state_vector_v1(&mut self, node_id: NodeId) -> Result<Vec<u8>> {
        self.ensure_loaded(node_id)?;
        let doc = self.docs.get(&node_id).expect("inserted above");
        let txn = doc.transact();
        Ok(txn.state_vector().encode_v1())
    }

    fn apply_update(&mut self, node_id: NodeId, payload: &[u8]) -> Result<()> {
        self.ensure_loaded(node_id)?;
        let doc = self.docs.get(&node_id).expect("inserted above");
        let upd = Update::decode_v1(payload).context("decode incoming content update")?;
        doc.transact_mut().apply_update(upd);
        Ok(())
    }

    fn persist(&self, node_id: NodeId) -> Result<()> {
        let doc = self
            .docs
            .get(&node_id)
            .ok_or_else(|| anyhow::anyhow!("persist: subdoc {:?} not loaded", node_id))?;
        let bytes = {
            let txn = doc.transact();
            txn.encode_state_as_update_v1(&StateVector::default())
        };
        let path = Self::content_file(&self.content_dir, node_id);
        atomic_write(&path, &bytes)
    }

    fn current_text(&self, node_id: NodeId) -> Option<String> {
        let doc = self.docs.get(&node_id)?;
        let text = doc.get_or_insert_text("text");
        let txn = doc.transact();
        Some(text.get_string(&txn))
    }

    /// Replace this node's Y.Text body with `new_body` using a minimal
    /// edit (common-prefix / common-suffix trim). Returns the encoded
    /// Y.Doc update delta for this transaction, suitable for broadcast
    /// as a `MSG_UPDATE`. Returns `None` if no change was needed.
    fn replace_text(&mut self, node_id: NodeId, new_body: &str) -> Result<Option<Vec<u8>>> {
        self.ensure_loaded(node_id)?;
        let doc = self.docs.get(&node_id).expect("loaded above");
        let text = doc.get_or_insert_text("text");
        let pre_sv = doc.transact().state_vector();
        let old = text.get_string(&doc.transact());
        if old == new_body {
            return Ok(None);
        }
        let (prefix, old_mid_len, new_mid) = compute_minimal_edit(&old, new_body);
        {
            let mut txn = doc.transact_mut();
            if old_mid_len > 0 {
                text.remove_range(&mut txn, prefix, old_mid_len);
            }
            if !new_mid.is_empty() {
                text.insert(&mut txn, prefix, &new_mid);
            }
        }
        let update = {
            let txn = doc.transact();
            txn.encode_state_as_update_v1(&pre_sv)
        };
        Ok(Some(update))
    }
}

/// Minimal insert/delete that turns `old` into `new`, expressed as
/// (prefix_byte_index, old_middle_byte_len, new_middle_string). All
/// byte offsets sit on UTF-8 char boundaries in both strings.
fn compute_minimal_edit(old: &str, new: &str) -> (u32, u32, String) {
    let common_head = old
        .as_bytes()
        .iter()
        .zip(new.as_bytes().iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut prefix = common_head;
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }

    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    let common_tail = old
        .as_bytes()
        .iter()
        .rev()
        .zip(new.as_bytes().iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let mut suffix = common_tail;
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }

    let old_mid_len = (old.len() - prefix - suffix) as u32;
    let new_mid = new[prefix..new.len() - suffix].to_string();
    (prefix as u32, old_mid_len, new_mid)
}

fn content_doc_id(node_id: NodeId) -> String {
    format!("content:{}", node_id.to_string_hyphenated())
}

fn parse_content_doc_id(doc_id: &str) -> Option<NodeId> {
    let rest = doc_id.strip_prefix("content:")?;
    NodeId::parse_str(rest)
}

/// Process an inbound `MSG_BLOB_UPDATE`. `doc_id` is the hex hash the
/// server echoes back on a request reply; `payload` is the blob bytes.
/// We verify the hash before trusting the write — a corrupted blob
/// never touches the store.
fn handle_inbound_blob(doc_id: &str, payload: &[u8], blobs: &BlobStore) -> Result<()> {
    if payload.len() > MAX_BLOB_SIZE {
        anyhow::bail!(
            "inbound blob {} is {} bytes, exceeds MAX_BLOB_SIZE {}",
            doc_id,
            payload.len(),
            MAX_BLOB_SIZE
        );
    }
    blobs.insert_verified(doc_id, payload).with_context(|| {
        format!("verifying inbound blob {} ({} bytes)", doc_id, payload.len())
    })?;
    debug!(
        blob_hash = doc_id,
        bytes = payload.len(),
        "stored inbound blob"
    );
    Ok(())
}

/// For each live Binary entry in the manifest projection whose blob we
/// don't yet have locally and haven't already requested this session,
/// send a `MSG_BLOB_REQUEST`. The server replies with `MSG_BLOB_UPDATE`
/// over the same connection.
async fn request_missing_blobs(
    write: &mut WsSink,
    manifest: &Manifest,
    blobs: &BlobStore,
    requested: &mut HashSet<String>,
) -> Result<()> {
    let proj = project(manifest);
    let mut sent = 0usize;
    for entry in proj.by_path.values() {
        if entry.kind != NodeKind::Binary {
            continue;
        }
        let Some(hash) = entry.blob_hash.as_deref() else {
            continue;
        };
        if blobs.has(hash) || requested.contains(hash) {
            continue;
        }
        // Server's handle_blob_request parses the payload as utf8 hash.
        // We echo the hash in doc_id too so the reply is self-identifying.
        let frame = encode_message(MSG_BLOB_REQUEST, hash, hash.as_bytes());
        write
            .send(WsMessage::Binary(frame.into()))
            .await
            .context("send blob request")?;
        requested.insert(hash.to_string());
        sent += 1;
    }
    if sent > 0 {
        debug!("sent {} MSG_BLOB_REQUEST frames", sent);
    }
    Ok(())
}

/// For every live Text entry in the manifest projection not yet tracked
/// in `subscribed`, send a content `MSG_SYNC_STEP_1`. The server replies
/// with `MSG_SYNC_STEP_2` carrying any updates we're missing.
async fn subscribe_new_text_content(
    write: &mut WsSink,
    manifest: &Manifest,
    content: &mut ContentStore,
    subscribed: &mut HashSet<NodeId>,
) -> Result<()> {
    let proj = project(manifest);
    let mut sent = 0usize;
    for entry in proj.by_path.values() {
        if entry.kind != NodeKind::Text || subscribed.contains(&entry.id) {
            continue;
        }
        let sv_bytes = content.state_vector_v1(entry.id)?;
        let frame = encode_message(MSG_SYNC_STEP_1, &content_doc_id(entry.id), &sv_bytes);
        write
            .send(WsMessage::Binary(frame.into()))
            .await
            .context("send content STEP_1")?;
        subscribed.insert(entry.id);
        sent += 1;
    }
    if sent > 0 {
        debug!("sent content STEP_1 for {} new text entries", sent);
    }
    Ok(())
}

/// Write the current Y.Text body of `node_id` through to the file at the
/// path currently projected for that node. No-op (with a warning log) if
/// the node has no projection path or the path fails the safety check.
fn flush_content_to_disk(
    folder: &Path,
    manifest: &Manifest,
    content: &ContentStore,
    node_id: NodeId,
) -> Result<()> {
    let proj = project(manifest);
    let Some(entry) = proj.by_id.get(&node_id) else {
        debug!("flush_content_to_disk: no projection entry for {:?}", node_id);
        return Ok(());
    };
    if entry.kind != NodeKind::Text {
        return Ok(());
    }
    if is_unsafe_relative_path(&entry.path) {
        warn!("unsafe projection path on content flush: {:?}", entry.path);
        return Ok(());
    }
    let full = folder.join(&entry.path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {} for content flush", parent.display()))?;
    }
    let body = content.current_text(node_id).unwrap_or_default();
    atomic_write(&full, body.as_bytes())
        .with_context(|| format!("atomic_write to {}", full.display()))?;
    debug!(
        bytes = body.len(),
        path = %entry.path,
        "flushed content subdoc to disk"
    );
    Ok(())
}

/// Shared atomic-write helper (tmp + fsync + rename), used by both the
/// manifest persist path and content-subdoc persistence. Creates parent
/// directories as needed.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("create tmp {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write tmp {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync tmp {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Projection → disk reconcile (create-only, Phase 3.3a subset)
// ---------------------------------------------------------------------------

/// Walk the manifest projection and create any missing entries on disk.
///
/// Rules, intentionally conservative:
///   * Parent directory chain for every projected path is `mkdir_p`ed.
///   * Missing **Text** files get created as empty placeholders — their
///     real content arrives via content subdoc sync (Phase 3.3b).
///   * Missing **Binary** files are written IFF the referenced blob is
///     present in the local CAS store; otherwise we log and defer. The
///     request is fired separately by `request_missing_blobs`; the next
///     reconcile (triggered when the blob arrives) will materialize
///     the file.
///   * Existing files are never overwritten (no truncation, no rewrite).
///   * Files on disk that are *not* in the projection are **not deleted**
///     — team-lead explicitly asked for create-only during the first
///     pass so unsynced local state doesn't get clobbered.
fn reconcile_projection_to_disk(
    folder: &Path,
    manifest: &Manifest,
    blobs: &BlobStore,
) -> Result<()> {
    let proj = project(manifest);
    let mut created_dirs = 0usize;
    let mut created_text = 0usize;
    let mut created_binary = 0usize;
    let mut pending_binary = 0usize;

    for (path, entry) in &proj.by_path {
        if is_unsafe_relative_path(path) {
            warn!("skipping unsafe projection path {:?}", path);
            continue;
        }
        let full = folder.join(path);
        if let Some(parent) = full.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("mkdir -p {} for projected {:?}", parent.display(), path)
                })?;
                created_dirs += 1;
            }
        }
        match entry.kind {
            NodeKind::Text => {
                if !full.exists() {
                    fs::File::create(&full).with_context(|| {
                        format!("create empty text placeholder {}", full.display())
                    })?;
                    created_text += 1;
                }
            }
            NodeKind::Binary => {
                if full.exists() {
                    continue;
                }
                let Some(hash) = entry.blob_hash.as_deref() else {
                    // A Binary entry with no hash is a manifest-level
                    // bug on the emitting peer; nothing we can write.
                    debug!("binary {:?} has no blob_hash, skipping", path);
                    continue;
                };
                if !blobs.has(hash) {
                    pending_binary += 1;
                    continue;
                }
                let bytes = blobs
                    .read(hash)
                    .with_context(|| format!("read blob {} for {:?}", hash, path))?;
                atomic_write(&full, &bytes).with_context(|| {
                    format!("materialize binary {} from blob {}", full.display(), hash)
                })?;
                created_binary += 1;
            }
            NodeKind::Directory => {
                // Emergent — directories never appear in projection.by_path.
            }
        }
    }

    if created_dirs + created_text + created_binary + pending_binary > 0 {
        info!(
            created_dirs,
            created_text_placeholders = created_text,
            created_binary,
            pending_binary,
            "reconciled projection → disk"
        );
    }
    Ok(())
}

/// Defensive check: projection paths are meant to be vault-relative, but
/// a malicious or buggy peer could produce something like `../etc/passwd`.
/// Refuse absolute paths and any `..` segment before we hand the string
/// to `folder.join`.
fn is_unsafe_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return true;
    }
    p.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    })
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

    #[test]
    fn reconcile_creates_dirs_and_text_placeholders_defers_binary_without_blob() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let (_bs_tmp, blobs) = fresh_blob_store();

        let actor = ActorId::new();
        let mut m = Manifest::new(actor);
        crate::v1::ops::create_text(&mut m, "top.md", 0).unwrap();
        crate::v1::ops::create_text(&mut m, "notes/deep/sub.md", 0).unwrap();
        // Binary present in manifest but blob is NOT in the local store.
        crate::v1::ops::create_binary(&mut m, "img/pic.png", "deadbeef", 1024).unwrap();

        reconcile_projection_to_disk(folder, &m, &blobs).unwrap();

        assert!(folder.join("top.md").exists(), "top.md should exist");
        assert_eq!(
            fs::metadata(folder.join("top.md")).unwrap().len(),
            0,
            "text placeholder must be empty"
        );
        assert!(folder.join("notes/deep").is_dir(), "nested dirs mkdir_p'd");
        assert!(folder.join("notes/deep/sub.md").exists());
        assert!(folder.join("img").is_dir(), "binary parent dir still created");
        assert!(
            !folder.join("img/pic.png").exists(),
            "binary file NOT materialised without the blob"
        );
    }

    #[test]
    fn reconcile_materialises_binary_when_blob_is_local() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let (_bs_tmp, blobs) = fresh_blob_store();
        let bytes = b"\x89PNG\r\n\x1a\npretend png";
        let hash = blobs.insert_bytes(bytes).unwrap();

        let mut m = Manifest::new(ActorId::new());
        crate::v1::ops::create_binary(&mut m, "img/pic.png", &hash, bytes.len() as u64)
            .unwrap();

        reconcile_projection_to_disk(folder, &m, &blobs).unwrap();

        let on_disk = fs::read(folder.join("img/pic.png")).unwrap();
        assert_eq!(on_disk, bytes);
    }

    #[test]
    fn reconcile_never_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let (_bs_tmp, blobs) = fresh_blob_store();

        let local_bytes = b"local user edits - must not be clobbered";
        fs::write(folder.join("diary.md"), local_bytes).unwrap();

        let actor = ActorId::new();
        let mut m = Manifest::new(actor);
        crate::v1::ops::create_text(&mut m, "diary.md", 0).unwrap();

        reconcile_projection_to_disk(folder, &m, &blobs).unwrap();

        let on_disk = fs::read(folder.join("diary.md")).unwrap();
        assert_eq!(on_disk, local_bytes, "reconcile clobbered local edits");
    }

    #[test]
    fn reconcile_never_overwrites_existing_binary_file() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let (_bs_tmp, blobs) = fresh_blob_store();

        // Local bytes on disk disagree with the manifest+blob content.
        let local_bytes = b"local binary - keep me";
        fs::write(folder.join("img.bin"), local_bytes).unwrap();

        let remote_bytes = b"remote binary";
        let hash = blobs.insert_bytes(remote_bytes).unwrap();
        let mut m = Manifest::new(ActorId::new());
        crate::v1::ops::create_binary(&mut m, "img.bin", &hash, remote_bytes.len() as u64)
            .unwrap();

        reconcile_projection_to_disk(folder, &m, &blobs).unwrap();

        let on_disk = fs::read(folder.join("img.bin")).unwrap();
        assert_eq!(on_disk, local_bytes, "binary reconcile clobbered local file");
    }

    #[test]
    fn reconcile_rejects_unsafe_paths() {
        assert!(is_unsafe_relative_path(""));
        assert!(is_unsafe_relative_path("/etc/passwd"));
        assert!(is_unsafe_relative_path("../escape"));
        assert!(is_unsafe_relative_path("a/../b"));
        assert!(!is_unsafe_relative_path("a/b/c.md"));
        assert!(!is_unsafe_relative_path("file.md"));
    }

    #[test]
    fn content_doc_id_roundtrips() {
        let id = NodeId::new();
        let s = content_doc_id(id);
        assert!(s.starts_with("content:"));
        assert_eq!(parse_content_doc_id(&s), Some(id));
        assert_eq!(parse_content_doc_id("manifest"), None);
        assert_eq!(parse_content_doc_id("content:not-a-uuid"), None);
    }

    #[test]
    fn content_store_round_trip_via_disk() {
        let dir = tempfile::tempdir().unwrap();
        let content_dir = dir.path().join("content");

        // Peer A builds a subdoc with body, snapshots it.
        let node_id = NodeId::new();
        let peer_a = {
            let d = Doc::new();
            let t = d.get_or_insert_text("text");
            t.insert(&mut d.transact_mut(), 0, "hello crdt");
            let txn = d.transact();
            txn.encode_state_as_update_v1(&StateVector::default())
        };

        // Peer B: ContentStore applies the snapshot and persists.
        let mut store = ContentStore::new(content_dir.clone());
        store.apply_update(node_id, &peer_a).unwrap();
        store.persist(node_id).unwrap();
        assert_eq!(store.current_text(node_id).as_deref(), Some("hello crdt"));

        // Fresh store: reload from disk, text must match.
        let mut reopened = ContentStore::new(content_dir);
        reopened.ensure_loaded(node_id).unwrap();
        assert_eq!(
            reopened.current_text(node_id).as_deref(),
            Some("hello crdt")
        );
    }

    #[test]
    fn flush_content_writes_projected_path() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let content_dir = folder.join(".syncline/content");

        let mut m = Manifest::new(ActorId::new());
        let nid = crate::v1::ops::create_text(&mut m, "notes/hi.md", 0).unwrap();

        let update_bytes = {
            let d = Doc::new();
            let t = d.get_or_insert_text("text");
            t.insert(&mut d.transact_mut(), 0, "Hello, world!");
            let txn = d.transact();
            txn.encode_state_as_update_v1(&StateVector::default())
        };

        let mut store = ContentStore::new(content_dir);
        store.apply_update(nid, &update_bytes).unwrap();

        flush_content_to_disk(folder, &m, &store, nid).unwrap();
        let written = fs::read_to_string(folder.join("notes/hi.md")).unwrap();
        assert_eq!(written, "Hello, world!");
    }

    #[test]
    fn compute_minimal_edit_identical() {
        let (p, old_len, new_mid) = compute_minimal_edit("hello", "hello");
        assert_eq!(p, 5);
        assert_eq!(old_len, 0);
        assert_eq!(new_mid, "");
    }

    #[test]
    fn compute_minimal_edit_middle_replace() {
        // "Hello WORLD!" -> "Hello rust!" — common prefix "Hello ", common suffix "!"
        let (p, old_len, new_mid) = compute_minimal_edit("Hello WORLD!", "Hello rust!");
        assert_eq!(p, 6);
        assert_eq!(old_len, 5);
        assert_eq!(new_mid, "rust");
    }

    #[test]
    fn compute_minimal_edit_append_and_delete() {
        let (p, old_len, new_mid) = compute_minimal_edit("abc", "abcdef");
        assert_eq!(p, 3);
        assert_eq!(old_len, 0);
        assert_eq!(new_mid, "def");

        let (p2, old_len2, new_mid2) = compute_minimal_edit("abcdef", "abc");
        assert_eq!(p2, 3);
        assert_eq!(old_len2, 3);
        assert_eq!(new_mid2, "");
    }

    #[test]
    fn compute_minimal_edit_respects_utf8_boundaries() {
        // "Hi ★!" vs "Hi ★?" — the only difference is the last byte
        // of the trailing ASCII. Prefix and suffix must not bisect the
        // 3-byte UTF-8 sequence of '★'.
        let (p, old_len, new_mid) = compute_minimal_edit("Hi ★!", "Hi ★?");
        assert_eq!(old_len, 1, "should delete only the '!'");
        assert_eq!(new_mid, "?");
        // prefix should land on the byte boundary just before '!' in old.
        assert!("Hi ★!".is_char_boundary(p as usize));
        assert!("Hi ★?".is_char_boundary(p as usize));
    }

    #[test]
    fn replace_text_produces_applicable_update() {
        let node_id = NodeId::new();
        let mut store = ContentStore::new(std::env::temp_dir().join("syncline_test_replace"));
        // Seed from empty to "foo".
        let u1 = store.replace_text(node_id, "foo").unwrap().unwrap();
        assert_eq!(store.current_text(node_id).as_deref(), Some("foo"));

        // Evolve to "food".
        let u2 = store.replace_text(node_id, "food").unwrap().unwrap();
        assert_eq!(store.current_text(node_id).as_deref(), Some("food"));

        // A peer that applies both updates on top of an empty doc must
        // converge to the same text.
        let peer = Doc::new();
        let t = peer.get_or_insert_text("text");
        peer.transact_mut()
            .apply_update(Update::decode_v1(&u1).unwrap());
        peer.transact_mut()
            .apply_update(Update::decode_v1(&u2).unwrap());
        assert_eq!(t.get_string(&peer.transact()), "food");
    }

    #[test]
    fn replace_text_noop_when_unchanged() {
        let mut store = ContentStore::new(std::env::temp_dir().join("syncline_test_noop"));
        let nid = NodeId::new();
        store.replace_text(nid, "same").unwrap();
        // Second call with identical body must produce no update.
        assert!(store.replace_text(nid, "same").unwrap().is_none());
    }

    #[test]
    fn flush_content_noop_when_node_not_in_projection() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let m = Manifest::new(ActorId::new());
        let store = ContentStore::new(folder.join(".syncline/content"));

        // Node exists only in the caller's head, not in the manifest.
        let orphan = NodeId::new();
        flush_content_to_disk(folder, &m, &store, orphan).unwrap();
        assert!(
            fs::read_dir(folder).unwrap().next().is_none(),
            "no files should be created for an unprojected node"
        );
    }

    // --- Phase 3.3c.2: outbound binary handling ------------------------------

    fn fresh_blob_store() -> (tempfile::TempDir, BlobStore) {
        let tmp = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(tmp.path().to_path_buf());
        (tmp, bs)
    }

    #[test]
    fn process_binary_creates_manifest_entry_and_stashes_blob() {
        let mut m = Manifest::new(ActorId::new());
        let proj = project(&m);
        let (_tmp, bs) = fresh_blob_store();
        let bytes = b"\x89PNG\r\n\x1a\npretend png";

        let outcome = process_binary_file("img/pic.png", bytes, &proj, &mut m, &bs).unwrap();
        let expected = hash_hex(bytes);

        match outcome {
            BinaryScanOutcome::Created { hash } => assert_eq!(hash, expected),
            other => panic!("expected Created, got {other:?}"),
        }
        assert!(bs.has(&expected), "blob must be in local store");

        // Manifest now has a Binary entry at that path with matching hash + size.
        let proj2 = project(&m);
        let entry = proj2
            .by_path
            .get("img/pic.png")
            .expect("projection should include img/pic.png");
        assert_eq!(entry.kind, NodeKind::Binary);
        assert_eq!(entry.blob_hash.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn process_binary_unchanged_when_hash_matches() {
        let mut m = Manifest::new(ActorId::new());
        let (_tmp, bs) = fresh_blob_store();
        let bytes = b"same png bytes";
        let h = hash_hex(bytes);
        crate::v1::ops::create_binary(&mut m, "a.png", &h, bytes.len() as u64).unwrap();
        bs.insert_bytes(bytes).unwrap();

        let proj = project(&m);
        let outcome = process_binary_file("a.png", bytes, &proj, &mut m, &bs).unwrap();
        assert!(matches!(outcome, BinaryScanOutcome::Unchanged));
    }

    #[test]
    fn process_binary_rehashes_on_content_change() {
        let mut m = Manifest::new(ActorId::new());
        let (_tmp, bs) = fresh_blob_store();
        let old = b"old png bytes";
        let h_old = hash_hex(old);
        crate::v1::ops::create_binary(&mut m, "a.png", &h_old, old.len() as u64).unwrap();
        bs.insert_bytes(old).unwrap();

        let new = b"new png bytes, different length and content";
        let h_new = hash_hex(new);
        let proj = project(&m);
        let outcome = process_binary_file("a.png", new, &proj, &mut m, &bs).unwrap();

        match outcome {
            BinaryScanOutcome::Rehashed { hash } => assert_eq!(hash, h_new),
            other => panic!("expected Rehashed, got {other:?}"),
        }
        assert!(bs.has(&h_new), "new blob must be stashed");
        let proj2 = project(&m);
        assert_eq!(
            proj2.by_path["a.png"].blob_hash.as_deref(),
            Some(h_new.as_str()),
            "manifest hash must point to the new blob"
        );
    }

    #[test]
    fn process_binary_skips_on_kind_mismatch() {
        let mut m = Manifest::new(ActorId::new());
        // A Text entry already exists at `ambiguous` — hostile local state.
        crate::v1::ops::create_text(&mut m, "ambiguous", 0).unwrap();
        let (_tmp, bs) = fresh_blob_store();
        let proj = project(&m);

        let outcome = process_binary_file("ambiguous", b"blob", &proj, &mut m, &bs).unwrap();
        match outcome {
            BinaryScanOutcome::Skipped(reason) => assert_eq!(reason, "kind_mismatch"),
            other => panic!("expected Skipped(kind_mismatch), got {other:?}"),
        }
    }

    #[test]
    fn handle_inbound_blob_rejects_hash_mismatch() {
        let (_tmp, blobs) = fresh_blob_store();
        let bytes = b"real payload";
        let wrong_hash = hash_hex(b"different payload");
        assert!(handle_inbound_blob(&wrong_hash, bytes, &blobs).is_err());
        // Store must not have been populated under either the claimed
        // or the real hash.
        assert!(!blobs.has(&wrong_hash));
        assert!(!blobs.has(&hash_hex(bytes)));
    }

    #[test]
    fn handle_inbound_blob_stores_when_hash_matches() {
        let (_tmp, blobs) = fresh_blob_store();
        let bytes = b"legit payload";
        let h = hash_hex(bytes);
        handle_inbound_blob(&h, bytes, &blobs).unwrap();
        assert!(blobs.has(&h));
        assert_eq!(blobs.read(&h).unwrap(), bytes);
    }

    #[test]
    fn handle_inbound_blob_rejects_oversize() {
        let (_tmp, blobs) = fresh_blob_store();
        // Pretend we got something bigger than MAX_BLOB_SIZE. Use a
        // small string; we override the size check by constructing a
        // vec of the right length.
        let bytes = vec![0u8; MAX_BLOB_SIZE + 1];
        let h = hash_hex(&bytes);
        assert!(handle_inbound_blob(&h, &bytes, &blobs).is_err());
        assert!(!blobs.has(&h));
    }

    #[test]
    fn process_binary_is_idempotent_across_repeat_calls() {
        let mut m = Manifest::new(ActorId::new());
        let (_tmp, bs) = fresh_blob_store();
        let bytes = b"payload";

        let proj = project(&m);
        let first = process_binary_file("x.bin", bytes, &proj, &mut m, &bs).unwrap();
        assert!(matches!(first, BinaryScanOutcome::Created { .. }));
        let n_after_first = m.live_entries().len();

        // A second scan with the same bytes must be a no-op: no duplicate
        // manifest entry, no state change.
        let proj2 = project(&m);
        let second = process_binary_file("x.bin", bytes, &proj2, &mut m, &bs).unwrap();
        assert!(matches!(second, BinaryScanOutcome::Unchanged));
        assert_eq!(m.live_entries().len(), n_after_first);
    }
}
