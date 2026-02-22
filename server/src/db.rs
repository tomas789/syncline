use anyhow::Result;
use sqlx::{sqlite::SqlitePool, Executor, Pool, Row, Sqlite};
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

        // Ensure table exists
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

        Ok(Self { pool })
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
}
