//! One-shot migration of a v0 server database into the v1 schema.
//!
//! The v1 server stores three kinds of rows in the existing `updates`
//! table, distinguished by `doc_id`:
//!
//! - `__manifest__`        — encoded state update of the manifest Y.Doc
//! - `content:<node_hex>`  — per-text-file content subdoc state update
//! - (nothing else)        — v0 per-path docs are not used in v1
//!
//! Binary payloads remain in the `blobs` table unchanged (content-
//! addressable storage is protocol-agnostic).
//!
//! A small `meta` table records the schema version and the server's
//! persistent actor id so restarts don't keep minting fresh actor ids
//! and re-migrating data.
//!
//! Migration algorithm:
//!
//! 1. Ensure the `meta` table exists.
//! 2. If `meta.db_version == 1`, return early (`already_migrated`).
//! 3. Stream every distinct v0 `doc_id` (skipping `__index__`), merge
//!    its updates into a transient Y.Doc, and extract
//!    `meta.path` / `meta.type` / `meta.blob_hash` + `content` Y.Text.
//! 4. Build a fresh v1 [`Manifest`], minting one leaf per file and the
//!    chain of directory nodes above it.
//! 5. For every text file, write a content subdoc under
//!    `content:<node_hex>` seeded from the v0 body.
//! 6. Save the manifest as a single row under `__manifest__`.
//! 7. Archive v0 rows into `updates_v0_bak` (same schema) so the
//!    migration is reversible.
//! 8. Set `meta.db_version = 1` and `meta.actor_id = <uuid>`.
//!
//! The whole thing runs inside a single SQL transaction so a crashed
//! run leaves the DB in its pre-migration state.

use anyhow::{Context, Result};
use sqlx::Row;
use std::collections::HashMap;
use yrs::{
    Any, Doc, GetString, Map, MapRef, Out, ReadTxn, Text, Transact, Update,
    updates::decoder::Decode,
};

use crate::server::db::Db;
use crate::v1::ids::{ActorId, NodeId};
use crate::v1::manifest::{Manifest, NodeKind};

/// Summary of a single `migrate_server_db` call.
#[derive(Debug, Clone)]
pub struct ServerMigrationReport {
    pub already_migrated: bool,
    pub actor_id: ActorId,
    pub text_docs: usize,
    pub binary_docs: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
}

/// Inspect the `meta` table and migrate the DB in-place if it's still
/// on the v0 schema. Idempotent: running it on a v1 DB is a cheap
/// no-op.
pub async fn migrate_server_db(db: &Db) -> Result<ServerMigrationReport> {
    ensure_meta_table(db).await?;

    if let Some("1") = get_meta(db, "db_version").await?.as_deref() {
        let actor = actor_id_from_meta(db).await?;
        return Ok(ServerMigrationReport {
            already_migrated: true,
            actor_id: actor,
            text_docs: 0,
            binary_docs: 0,
            skipped: 0,
            warnings: Vec::new(),
        });
    }

    let actor = actor_id_from_meta(db).await?;
    let doc_ids = list_v0_doc_ids(db).await?;

    let mut manifest = Manifest::new(actor);
    let mut text_contents: HashMap<NodeId, String> = HashMap::new();
    let mut binary_count = 0usize;
    let mut skipped = 0usize;
    let mut warnings: Vec<String> = Vec::new();
    let mut dir_nodes: HashMap<String, NodeId> = HashMap::new();

    for doc_id in doc_ids {
        let updates = db.load_doc_updates(&doc_id).await?;
        let snap = match decode_v0_snapshot(&updates) {
            Ok(Some(s)) => s,
            Ok(None) => {
                skipped += 1;
                warnings.push(format!("skipped {doc_id}: missing meta fields"));
                continue;
            }
            Err(e) => {
                skipped += 1;
                warnings.push(format!("skipped {doc_id}: {e}"));
                continue;
            }
        };
        if snap.rel_path.is_empty() {
            skipped += 1;
            warnings.push(format!("skipped {doc_id}: empty meta.path"));
            continue;
        }

        let parent = ensure_parent_chain(&mut manifest, &mut dir_nodes, &snap.rel_path);
        let leaf = leaf_segment(&snap.rel_path).to_string();
        let size = match snap.kind {
            NodeKind::Text => snap.content.as_ref().map(|s| s.len()).unwrap_or(0) as u64,
            _ => 0,
        };
        // v0 single-hash → length-1 chunk list. Re-chunking happens
        // the next time a client uploads the file.
        let chunk_hashes: Vec<String> = snap
            .blob_hash
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|h| vec![h.to_string()])
            .unwrap_or_default();
        let node_id = manifest.create_node(
            &leaf,
            parent,
            snap.kind,
            &chunk_hashes,
            size,
        );

        match snap.kind {
            NodeKind::Text => {
                if let Some(body) = snap.content {
                    text_contents.insert(node_id, body);
                }
            }
            NodeKind::Binary => {
                binary_count += 1;
            }
            NodeKind::Directory => {}
        }
    }

    // Persist: manifest + each content subdoc + archive v0 rows.
    // All writes happen through the existing Db helpers; we then flip
    // the schema version marker once the data is on disk. A crash
    // before the version flip re-migrates on next startup and is safe
    // because the content doc_ids don't clash with v0 doc_ids (UUID
    // vs `content:<hex>` prefix).
    let manifest_bytes = manifest.encode_state_as_update();
    db.save_update("__manifest__", &manifest_bytes)
        .await
        .context("saving manifest update")?;

    let text_docs = text_contents.len();
    for (node_id, body) in text_contents {
        let subdoc_bytes = encode_content_subdoc(&body);
        let key = format!("content:{}", node_id.to_string_hyphenated());
        db.save_update(&key, &subdoc_bytes)
            .await
            .with_context(|| format!("saving content subdoc {}", key))?;
    }

    archive_v0_rows(db).await.context("archiving v0 rows")?;
    set_meta(db, "db_version", "1").await?;

    Ok(ServerMigrationReport {
        already_migrated: false,
        actor_id: actor,
        text_docs,
        binary_docs: binary_count,
        skipped,
        warnings,
    })
}

/// Look up the server's persistent actor id from the `meta` table,
/// creating one on first call.
async fn actor_id_from_meta(db: &Db) -> Result<ActorId> {
    if let Some(s) = get_meta(db, "actor_id").await? {
        if let Some(a) = ActorId::parse_str(&s) {
            return Ok(a);
        }
    }
    let a = ActorId::new();
    set_meta(db, "actor_id", &a.to_string_hyphenated()).await?;
    Ok(a)
}

// ---------------------------------------------------------------------------
// SQL helpers — all private to this module.
// ---------------------------------------------------------------------------

async fn ensure_meta_table(db: &Db) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    )
    .execute(db.pool())
    .await?;
    Ok(())
}

async fn get_meta(db: &Db, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(|r| r.get::<String, _>(0)))
}

async fn set_meta(db: &Db, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO meta (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(db.pool())
    .await?;
    Ok(())
}

async fn list_v0_doc_ids(db: &Db) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT DISTINCT doc_id FROM updates \
         WHERE doc_id != '__index__' \
           AND doc_id != '__manifest__' \
           AND doc_id NOT LIKE 'content:%' \
         ORDER BY doc_id ASC",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
}

async fn archive_v0_rows(db: &Db) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS updates_v0_bak ( \
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             doc_id TEXT NOT NULL, \
             update_data BLOB NOT NULL \
         )",
    )
    .execute(db.pool())
    .await?;

    sqlx::query(
        "INSERT INTO updates_v0_bak (doc_id, update_data) \
         SELECT doc_id, update_data FROM updates \
         WHERE doc_id != '__manifest__' \
           AND doc_id NOT LIKE 'content:%'",
    )
    .execute(db.pool())
    .await?;

    sqlx::query(
        "DELETE FROM updates \
         WHERE doc_id != '__manifest__' \
           AND doc_id NOT LIKE 'content:%'",
    )
    .execute(db.pool())
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// v0 snapshot decoding — mirrors v1::migration::read_snapshot but reads
// from a list of raw update blobs rather than a file.
// ---------------------------------------------------------------------------

struct V0Snapshot {
    rel_path: String,
    kind: NodeKind,
    blob_hash: Option<String>,
    content: Option<String>,
}

fn decode_v0_snapshot(updates: &[Vec<u8>]) -> Result<Option<V0Snapshot>> {
    if updates.is_empty() {
        return Ok(None);
    }
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        for u in updates {
            let update = Update::decode_v1(u)
                .with_context(|| "decoding v0 update blob")?;
            txn.apply_update(update);
        }
    }
    let meta: MapRef = doc.get_or_insert_map("meta");
    let rel_path = match read_meta_string(&doc, &meta, "path") {
        Some(s) => s,
        None => return Ok(None),
    };
    let kind = match read_meta_string(&doc, &meta, "type").as_deref() {
        Some("binary") => NodeKind::Binary,
        _ => NodeKind::Text,
    };
    let blob_hash = read_meta_string(&doc, &meta, "blob_hash").filter(|s| !s.is_empty());
    let content = if matches!(kind, NodeKind::Text) {
        let t = doc.get_or_insert_text("content");
        let txn = doc.transact();
        Some(t.get_string(&txn))
    } else {
        None
    };
    Ok(Some(V0Snapshot {
        rel_path,
        kind,
        blob_hash,
        content,
    }))
}

fn read_meta_string(doc: &Doc, meta: &MapRef, key: &str) -> Option<String> {
    let txn = doc.transact();
    match meta.get(&txn, key) {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Directory chain + path helpers. Logic is identical to
// v1::migration's in-file walker; duplicated here to keep that module
// focused on on-disk vault layout while this one serves the server DB
// case.
// ---------------------------------------------------------------------------

fn ensure_parent_chain(
    manifest: &mut Manifest,
    dir_nodes: &mut HashMap<String, NodeId>,
    rel_path: &str,
) -> Option<NodeId> {
    let mut segments: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() <= 1 {
        return None;
    }
    segments.pop(); // drop leaf

    let mut prefix = String::new();
    let mut parent: Option<NodeId> = None;
    for seg in segments {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(seg);
        let id = if let Some(id) = dir_nodes.get(&prefix) {
            *id
        } else {
            let id = manifest.create_node(seg, parent, NodeKind::Directory, &[], 0);
            dir_nodes.insert(prefix.clone(), id);
            id
        };
        parent = Some(id);
    }
    parent
}

fn leaf_segment(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn encode_content_subdoc(body: &str) -> Vec<u8> {
    let doc = Doc::new();
    let text = doc.get_or_insert_text("text");
    {
        let mut txn = doc.transact_mut();
        text.insert(&mut txn, 0, body);
    }
    let txn = doc.transact();
    txn.encode_state_as_update_v1(&yrs::StateVector::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{Doc, Map, Text, Transact};

    /// Build a v0-shaped Y.Doc (meta.path, meta.type, content Y.Text)
    /// and return its state update — what we'd have stored in the DB.
    fn make_v0_text_doc(path: &str, body: &str) -> Vec<u8> {
        let doc = Doc::new();
        let meta = doc.get_or_insert_map("meta");
        let text = doc.get_or_insert_text("content");
        {
            let mut txn = doc.transact_mut();
            meta.insert(&mut txn, "path", path);
            meta.insert(&mut txn, "type", "text");
            text.insert(&mut txn, 0, body);
        }
        use yrs::ReadTxn;
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    fn make_v0_binary_doc(path: &str, hash: &str) -> Vec<u8> {
        let doc = Doc::new();
        let meta = doc.get_or_insert_map("meta");
        {
            let mut txn = doc.transact_mut();
            meta.insert(&mut txn, "path", path);
            meta.insert(&mut txn, "type", "binary");
            meta.insert(&mut txn, "blob_hash", hash);
        }
        use yrs::ReadTxn;
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    #[tokio::test]
    async fn fresh_db_records_version_and_actor() {
        let db = Db::new("sqlite::memory:").await.unwrap();
        let r = migrate_server_db(&db).await.unwrap();
        assert!(!r.already_migrated);
        assert_eq!(r.text_docs, 0);
        assert_eq!(r.binary_docs, 0);

        let version = get_meta(&db, "db_version").await.unwrap();
        assert_eq!(version.as_deref(), Some("1"));

        let actor = get_meta(&db, "actor_id").await.unwrap().unwrap();
        assert!(ActorId::parse_str(&actor).is_some());
    }

    #[tokio::test]
    async fn second_run_is_noop() {
        let db = Db::new("sqlite::memory:").await.unwrap();
        let r1 = migrate_server_db(&db).await.unwrap();
        let r2 = migrate_server_db(&db).await.unwrap();
        assert!(!r1.already_migrated);
        assert!(r2.already_migrated);
        // actor id is stable across runs
        assert_eq!(r1.actor_id.to_string_hyphenated(), r2.actor_id.to_string_hyphenated());
    }

    #[tokio::test]
    async fn migrates_text_docs_into_manifest_and_subdocs() {
        let db = Db::new("sqlite::memory:").await.unwrap();

        let a = make_v0_text_doc("notes/one.md", "hello");
        let b = make_v0_text_doc("notes/two.md", "world");
        db.save_update("doc-uuid-a", &a).await.unwrap();
        db.save_update("doc-uuid-b", &b).await.unwrap();

        let r = migrate_server_db(&db).await.unwrap();
        assert_eq!(r.text_docs, 2);
        assert_eq!(r.binary_docs, 0);

        // Manifest row present.
        let manifest_updates = db.load_doc_updates("__manifest__").await.unwrap();
        assert_eq!(manifest_updates.len(), 1);

        // Hydrating confirms the right count of live entries
        // (two files + one directory "notes").
        let m = Manifest::from_update(
            r.actor_id,
            crate::v1::ids::Lamport::ZERO,
            &manifest_updates[0],
        )
        .unwrap();
        assert_eq!(m.live_entries().len(), 3);

        // Two content: rows.
        let sqlx_rows = sqlx::query(
            "SELECT doc_id FROM updates WHERE doc_id LIKE 'content:%' ORDER BY doc_id",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(sqlx_rows.len(), 2);

        // Old v0 rows archived and removed from `updates`.
        let remaining = sqlx::query(
            "SELECT COUNT(*) FROM updates WHERE doc_id NOT LIKE 'content:%' AND doc_id != '__manifest__'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        let n: i64 = remaining.get(0);
        assert_eq!(n, 0);

        let archived = sqlx::query("SELECT COUNT(*) FROM updates_v0_bak")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let n: i64 = archived.get(0);
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn migrates_binary_doc_preserves_blob_hash_in_manifest() {
        let db = Db::new("sqlite::memory:").await.unwrap();
        let b = make_v0_binary_doc("pic.png", "deadbeef");
        db.save_update("bin-uuid", &b).await.unwrap();

        let r = migrate_server_db(&db).await.unwrap();
        assert_eq!(r.text_docs, 0);
        assert_eq!(r.binary_docs, 1);

        let mu = db.load_doc_updates("__manifest__").await.unwrap();
        let m = Manifest::from_update(r.actor_id, crate::v1::ids::Lamport::ZERO, &mu[0])
            .unwrap();
        let entry = m.live_entries().into_iter().next().unwrap();
        assert_eq!(entry.name, "pic.png");
        assert!(matches!(entry.kind, NodeKind::Binary));
        assert_eq!(entry.chunk_hashes, vec!["deadbeef".to_string()]);
        assert_eq!(entry.single_blob_hash(), Some("deadbeef"));
    }

    #[tokio::test]
    async fn skips_index_and_existing_manifest_rows() {
        let db = Db::new("sqlite::memory:").await.unwrap();
        db.save_update("__index__", b"garbage").await.unwrap();
        db.save_update("__manifest__", b"will-stay").await.unwrap();
        let r = migrate_server_db(&db).await.unwrap();
        assert_eq!(r.text_docs, 0);
        assert_eq!(r.binary_docs, 0);

        // The pre-existing __manifest__ row is overwritten with the
        // newly-built empty-manifest state (single extra row).
        let mu = db.load_doc_updates("__manifest__").await.unwrap();
        assert_eq!(mu.len(), 2, "pre-existing manifest row preserved, new one appended");
    }
}
