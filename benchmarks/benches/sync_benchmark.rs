use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tempfile::NamedTempFile;

// Client dependencies
use client_folder::diff::apply_diff_to_yrs;
use yrs::{Doc, Text, Transact};

// Server dependencies
use server::db::Db;

fn bench_apply_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("Client Apply Diff");

    // Setup doc for diffing
    let old_text = "apple ".repeat(100);
    // Add deletion and insertion
    let new_text = "banana ".repeat(50) + &"apple ".repeat(50);

    group.bench_function("apply_diff_to_yrs 100 words", |b| {
        b.iter(|| {
            let doc = Doc::new();
            let text_ref = doc.get_or_insert_text("content");
            {
                let mut txn = doc.transact_mut();
                text_ref.insert(&mut txn, 0, &old_text);
            }
            apply_diff_to_yrs(
                black_box(&doc),
                black_box(&text_ref),
                black_box(&old_text),
                black_box(&new_text),
            );
        })
    });

    let old_large = "data ".repeat(10000);
    let new_large = "data ".repeat(5000) + "changed " + &"data ".repeat(4999);

    group.bench_function("apply_diff_to_yrs 10000 words", |b| {
        b.iter(|| {
            let doc = Doc::new();
            let text_ref = doc.get_or_insert_text("content");
            {
                let mut txn = doc.transact_mut();
                text_ref.insert(&mut txn, 0, &old_large);
            }
            apply_diff_to_yrs(
                black_box(&doc),
                black_box(&text_ref),
                black_box(&old_large),
                black_box(&new_large),
            );
        })
    });

    group.finish();
}

async fn setup_db() -> Db {
    Db::new("sqlite::memory:").await.unwrap()
}

fn bench_server_db(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("Server DB");

    group.bench_function("save_and_get_updates", |b| {
        b.to_async(&rt).iter(|| async {
            let db = setup_db().await;

            // Use dummy update bytes to bypass yrs version mismatch
            let update_data = b"dummy_update_data".to_vec();

            db.save_update("doc1", black_box(&update_data))
                .await
                .unwrap();

            let _sync_data = db.load_doc_updates("doc1").await.unwrap();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_apply_diff, bench_server_db);
criterion_main!(benches);
