use anyhow::Result;
use sqlx::{Executor, Pool, Row, Sqlite, sqlite::SqlitePool};
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

#[derive(Clone)]
pub struct Db {
    pool: Pool<Sqlite>,
}

impl Db {
    pub async fn new(connection_string: &str) -> Result<Self> {
        let pool = SqlitePool::connect(connection_string).await?;

        let mut conn = pool.acquire().await?;

        // Ensure tables exist
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS updates (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                doc_id TEXT NOT NULL,
                update_data BLOB NOT NULL
            );
            "#,
        )
        .await?;

        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS blobs (
                hash TEXT PRIMARY KEY,
                data BLOB NOT NULL,
                size INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            "#,
        )
        .await?;

        Ok(Self { pool })
    }

    /// Raw connection pool — used by server-side migration code that
    /// needs to run ad-hoc DDL / schema queries the typed helpers here
    /// don't expose.
    pub(crate) fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }

    pub async fn save_update(&self, doc_id: &str, update: &[u8]) -> Result<()> {
        sqlx::query("INSERT INTO updates (doc_id, update_data) VALUES (?, ?)")
            .bind(doc_id)
            .bind(update)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn load_doc_updates(&self, doc_id: &str) -> Result<Vec<Vec<u8>>> {
        let rows = sqlx::query("SELECT update_data FROM updates WHERE doc_id = ? ORDER BY id ASC")
            .bind(doc_id)
            .fetch_all(&self.pool)
            .await?;

        let mut updates = Vec::new();
        for row in rows {
            updates.push(row.get(0));
        }

        Ok(updates)
    }

    pub async fn count_docs(&self) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(DISTINCT doc_id) FROM updates WHERE doc_id != '__index__'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Encode the current state vector for a doc as v1 bytes.
    ///
    /// Used by the server to advertise what it has so the client can
    /// reply with the inverse diff (its updates the server is missing).
    /// Without this the sync handshake is one-way: a client whose subdoc
    /// has updates we never observed has no way to push them back after
    /// reconnect.
    pub async fn get_doc_state_vector(&self, doc_id: &str) -> Result<Vec<u8>> {
        use yrs::Doc;
        use yrs::ReadTxn;
        use yrs::Transact;
        use yrs::Update;
        use yrs::updates::decoder::Decode;
        use yrs::updates::encoder::Encode;

        let all_updates = self.load_doc_updates(doc_id).await?;
        tokio::task::spawn_blocking(move || {
            let doc = Doc::new();
            if !all_updates.is_empty() {
                let mut txn = doc.transact_mut();
                for update_data in all_updates {
                    if let Ok(u) = Update::decode_v1(&update_data) {
                        txn.apply_update(u);
                    }
                }
            }
            let txn = doc.transact();
            Ok(txn.state_vector().encode_v1())
        })
        .await
        .unwrap()
    }

    pub async fn get_all_updates_since(
        &self,
        doc_id: &str,
        since_sv: &StateVector,
    ) -> Result<Vec<u8>> {
        // In a real implementation, we might optimize this query based on vector clock.
        // For this PoC, we load all updates, merge them into a single update relative to the SV.
        // Or simply return all raw updates and let the client merge.
        // The most robust way for the server:
        // 1. Load full history for doc into a temporary Yrs doc.
        // 2. Compute the difference update based on `since_sv`.

        let all_updates = self.load_doc_updates(doc_id).await?;
        let since_sv = since_sv.clone();

        tokio::task::spawn_blocking(move || {
            if all_updates.is_empty() {
                let doc = Doc::new();
                let txn = doc.transact();
                return Ok(txn.encode_state_as_update_v1(&since_sv));
            }

            let doc = Doc::new();
            {
                let mut txn = doc.transact_mut();
                for update_data in all_updates {
                    if let Ok(u) = Update::decode_v1(&update_data) {
                        txn.apply_update(u);
                    }
                }

                let update_to_sync = txn.encode_state_as_update_v1(&since_sv);
                Ok(update_to_sync)
            }
        })
        .await
        .unwrap()
    }

    /// Store a binary blob by its SHA256 hash. Content-addressable: if the hash
    /// already exists the insert is silently ignored (deduplication).
    pub async fn save_blob(&self, hash: &str, data: &[u8]) -> Result<()> {
        let size = data.len() as i64;
        sqlx::query("INSERT OR IGNORE INTO blobs (hash, data, size) VALUES (?, ?, ?)")
            .bind(hash)
            .bind(data)
            .bind(size)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load a binary blob by its SHA256 hash. Returns None if not found.
    pub async fn load_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let row = sqlx::query("SELECT data FROM blobs WHERE hash = ?")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Check whether a blob with the given hash exists.
    pub async fn has_blob(&self, hash: &str) -> Result<bool> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM blobs WHERE hash = ?")
                .bind(hash)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0 > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{GetString, Text};

    #[tokio::test]
    async fn test_db_operations() {
        let db = Db::new("sqlite::memory:").await.unwrap();

        let doc_id = "test_doc";
        let fake_update1 = vec![1, 2, 3];
        let fake_update2 = vec![4, 5, 6];

        // Ensure table is empty
        let initial_updates = db.load_doc_updates(doc_id).await.unwrap();
        assert!(initial_updates.is_empty());

        // Test save_update
        db.save_update(doc_id, &fake_update1).await.unwrap();
        db.save_update(doc_id, &fake_update2).await.unwrap();

        // Test load_doc_updates
        let updates = db.load_doc_updates(doc_id).await.unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0], fake_update1);
        assert_eq!(updates[1], fake_update2);

        // Test empty doc load
        let empty_updates = db.load_doc_updates("nonexistent_doc").await.unwrap();
        assert!(empty_updates.is_empty());

        // Test get_all_updates_since with an empty history
        let sv = StateVector::default();
        let update = db
            .get_all_updates_since("nonexistent_doc", &sv)
            .await
            .unwrap();
        assert!(!update.is_empty()); // empty doc creates an empty update representation
    }

    #[tokio::test]
    async fn test_db_get_all_updates_since() {
        let db = Db::new("sqlite::memory:").await.unwrap();
        let doc_id = "yrs_doc";

        // Create a real Yrs doc, transact, and get updates
        let doc1 = Doc::new();
        let txt = doc1.get_or_insert_text("test");

        let update1 = {
            let mut txn = doc1.transact_mut();
            txt.insert(&mut txn, 0, "Hello!");
            txn.encode_state_as_update_v1(&StateVector::default())
        };

        let update2 = {
            let mut txn = doc1.transact_mut();
            txt.insert(&mut txn, 6, " World.");
            txn.encode_state_as_update_v1(&StateVector::default())
        };

        // Save valid yrs updates
        db.save_update(doc_id, &update1).await.unwrap();
        db.save_update(doc_id, &update2).await.unwrap();

        // Load sync update
        let empty_sv = StateVector::default();
        let sync_update = db.get_all_updates_since(doc_id, &empty_sv).await.unwrap();

        // Ensure the sync update yields a doc with the same final string
        let doc2 = Doc::new();
        let txt2 = doc2.get_or_insert_text("test");
        let mut txn2 = doc2.transact_mut();
        txn2.apply_update(Update::decode_v1(&sync_update).unwrap());

        assert_eq!(txt2.get_string(&txn2), "Hello! World.");
    }

    #[tokio::test]
    async fn test_save_and_load_blob() {
        let db = Db::new("sqlite::memory:").await.unwrap();

        let hash = "abc123def456";
        let data = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]; // PNG header

        // Initially not present
        assert!(!db.has_blob(hash).await.unwrap());
        assert!(db.load_blob(hash).await.unwrap().is_none());

        // Save it
        db.save_blob(hash, &data).await.unwrap();

        // Now present
        assert!(db.has_blob(hash).await.unwrap());
        let loaded = db.load_blob(hash).await.unwrap().unwrap();
        assert_eq!(loaded, data);
    }

    #[tokio::test]
    async fn test_blob_deduplication() {
        let db = Db::new("sqlite::memory:").await.unwrap();

        let hash = "dedup_hash";
        let data1 = vec![1, 2, 3];
        let data2 = vec![4, 5, 6]; // different data, same hash (shouldn't overwrite)

        db.save_blob(hash, &data1).await.unwrap();
        db.save_blob(hash, &data2).await.unwrap(); // INSERT OR IGNORE

        // Should still return the original data (first insert wins)
        let loaded = db.load_blob(hash).await.unwrap().unwrap();
        assert_eq!(loaded, data1);
    }

    #[tokio::test]
    async fn test_blob_not_found() {
        let db = Db::new("sqlite::memory:").await.unwrap();
        assert!(db.load_blob("nonexistent").await.unwrap().is_none());
        assert!(!db.has_blob("nonexistent").await.unwrap());
    }
}
