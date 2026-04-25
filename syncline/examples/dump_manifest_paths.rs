//! Dump all paths from a syncline server's manifest.
//!
//! Usage:
//!   cargo run --example dump_manifest_paths -- <db.sqlite>
//!
//! Reads every row from `updates` whose `doc_id = '__manifest__'`,
//! applies them to a Yrs doc, projects, and prints `path\tnode_id\tkind`.

use std::env;
use std::path::PathBuf;

use sqlx::Row;
use sqlx::sqlite::SqlitePoolOptions;
use syncline::v1::ids::ActorId;
use syncline::v1::manifest::Manifest;
use syncline::v1::projection::project;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let db_path = env::args().nth(1).expect("usage: dump_manifest_paths <db>");
    let db_path = PathBuf::from(db_path);

    let url = format!("sqlite://{}?mode=ro", db_path.display());
    let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await?;

    let rows = sqlx::query("SELECT update_data FROM updates WHERE doc_id = '__manifest__' ORDER BY id ASC")
        .fetch_all(&pool)
        .await?;
    eprintln!("{} manifest update rows", rows.len());

    // Use a fixed actor id; it doesn't affect projection.
    let actor = ActorId::new();
    let mut manifest = Manifest::new(actor);
    for row in &rows {
        let update: Vec<u8> = row.get("update_data");
        if let Err(e) = manifest.apply_update(&update) {
            eprintln!("apply_update failed: {e}");
        }
    }

    let proj = project(&manifest);
    let mut entries: Vec<_> = proj.by_path.values().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let filter = env::args().nth(2);
    let mut shown = 0usize;
    for e in &entries {
        if let Some(f) = filter.as_deref() {
            if !e.path.contains(f) {
                continue;
            }
        }
        let kind = match e.kind {
            syncline::v1::manifest::NodeKind::Text => "text",
            syncline::v1::manifest::NodeKind::Binary => "binary",
            syncline::v1::manifest::NodeKind::Directory => "dir",
        };
        let conflict = if e.is_conflict_copy { " [conflict]" } else { "" };
        println!("{}\t{}\t{:?}{}", e.path, kind, e.id, conflict);
        shown += 1;
    }
    eprintln!("{shown}/{} rows shown", entries.len());
    Ok(())
}
