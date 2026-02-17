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
        if all_updates.is_empty() {
            return Ok(Vec::new());
        }

        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            for update_data in all_updates {
                if let Ok(u) = Update::decode_v1(&update_data) {
                    txn.apply_update(u);
                }
            }

            let update_to_sync = txn.encode_state_as_update_v1(since_sv);
            Ok(update_to_sync)
        }
    }
}
