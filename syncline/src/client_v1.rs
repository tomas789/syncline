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
    MANIFEST_DOC_ID, MSG_MANIFEST_SYNC, MSG_SYNC_STEP_1, MSG_SYNC_STEP_2, MSG_UPDATE, MSG_VERSION,
    V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR, decode_message, encode_message,
};
use crate::v1::disk::{migrate_vault_on_disk, read_or_create_actor_id};
use crate::v1::ids::{ActorId, Lamport, NodeId};
use crate::v1::manifest::{Manifest, NodeKind};
use crate::v1::projection::project;
use crate::v1::sync::{
    decode_version_handshake, encode_version_handshake, handle_manifest_payload,
    manifest_step1_payload,
};
use anyhow::{Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message as WsMessage,
};
use tracing::{debug, error, info, warn};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Transact, Update};

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;

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
    let mut content = ContentStore::new(syncline_dir.join("content"));

    let mut attempt: u32 = 0;
    loop {
        match run_session(&url, &mut manifest, &mut content, &folder, &syncline_dir).await {
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
                    // Conservative reconcile: only create missing entries
                    // (dirs + empty text placeholders). Never deletes,
                    // never overwrites. Full content arrives via the
                    // content-subdoc STEP_1 we fire next.
                    if let Err(e) = reconcile_projection_to_disk(folder, manifest) {
                        error!("reconciling projection: {e}");
                    }
                    // Subscribe to any newly-appeared text content
                    // subdocs. Idempotent across repeated manifest
                    // applies via `content_subscribed`.
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
                    if let Err(e) = flush_content_to_disk(folder, manifest, content, node_id)
                    {
                        error!("write content to disk for {:?}: {e}", node_id);
                    }
                }
                MSG_SYNC_STEP_1 => {
                    // Server replies to our STEP_1 with STEP_2 or UPDATE,
                    // so receiving STEP_1 from the server is unexpected
                    // under v1. Log and ignore rather than echoing.
                    debug!("unexpected STEP_1 for {}", doc_id);
                }
                other => {
                    debug!("ignoring content frame msg_type={:#x} for {}", other, doc_id);
                }
            }
            continue;
        }

        debug!("dropping frame for unknown doc_id={}", doc_id);
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
    let bytes = {
        let txn = manifest.doc().transact();
        txn.encode_state_as_update_v1(&StateVector::default())
    };
    atomic_write(&syncline_dir.join("manifest.bin"), &bytes)
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
}

fn content_doc_id(node_id: NodeId) -> String {
    format!("content:{}", node_id.to_string_hyphenated())
}

fn parse_content_doc_id(doc_id: &str) -> Option<NodeId> {
    let rest = doc_id.strip_prefix("content:")?;
    NodeId::parse_str(rest)
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
/// Rules, intentionally conservative for 3.3a:
///   * Parent directory chain for every projected path is `mkdir_p`ed.
///   * Missing **Text** files get created as empty placeholders — their
///     real content arrives in 3.3b via content subdoc sync.
///   * Missing **Binary** files are **skipped**; blob fetch is 3.3c.
///   * Existing files are never overwritten (no truncation, no rewrite).
///   * Files on disk that are *not* in the projection are **not deleted**
///     — team-lead explicitly asked for create-only during the first
///     pass so unsynced local state doesn't get clobbered.
fn reconcile_projection_to_disk(folder: &Path, manifest: &Manifest) -> Result<()> {
    let proj = project(manifest);
    let mut created_dirs = 0usize;
    let mut created_text = 0usize;
    let mut skipped_binary = 0usize;

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
                if !full.exists() {
                    skipped_binary += 1;
                }
            }
            NodeKind::Directory => {
                // Emergent — directories never appear in projection.by_path.
            }
        }
    }

    if created_dirs + created_text + skipped_binary > 0 {
        info!(
            created_dirs,
            created_text_placeholders = created_text,
            skipped_binary_pending_3_3c = skipped_binary,
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
    use yrs::Text;

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
    fn reconcile_creates_dirs_and_text_placeholders_skips_binary() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();

        let actor = ActorId::new();
        let mut m = Manifest::new(actor);
        crate::v1::ops::create_text(&mut m, "top.md", 0).unwrap();
        crate::v1::ops::create_text(&mut m, "notes/deep/sub.md", 0).unwrap();
        crate::v1::ops::create_binary(&mut m, "img/pic.png", "deadbeef", 1024).unwrap();

        reconcile_projection_to_disk(folder, &m).unwrap();

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
            "binary file NOT materialised until 3.3c"
        );
    }

    #[test]
    fn reconcile_never_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();

        let local_bytes = b"local user edits - must not be clobbered";
        fs::write(folder.join("diary.md"), local_bytes).unwrap();

        let actor = ActorId::new();
        let mut m = Manifest::new(actor);
        crate::v1::ops::create_text(&mut m, "diary.md", 0).unwrap();

        reconcile_projection_to_disk(folder, &m).unwrap();

        let on_disk = fs::read(folder.join("diary.md")).unwrap();
        assert_eq!(on_disk, local_bytes, "reconcile clobbered local edits");
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
}
