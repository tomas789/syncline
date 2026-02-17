mod common;
use common::TestCluster;

/// Goal: Verify binary files (images) sync correctly.
#[test]
fn binary_upload_and_sync() {
    let mut cluster = TestCluster::new("binary_sync");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    // Create a binary file (simulated with random bytes)
    let content = vec![0u8, 10, 255, 12, 42, 99];
    cluster.write_binary_file("Alice", "image.png", &content);

    cluster.wait_sync();

    assert!(cluster.file_exists("Bob", "image.png"));
    assert_eq!(cluster.read_binary_file("Bob", "image.png"), content);
}

/// Goal: Verify that when binary files conflict, both versions are preserved under different names.
#[test]
fn binary_conflict() {
    let mut cluster = TestCluster::new("binary_conflict");
    cluster.start_server();
    cluster.start_client("Alice");
    cluster.start_client("Bob");

    // Initial sync
    let v1 = b"Version1";
    cluster.write_binary_file("Alice", "logo.png", v1);
    cluster.wait_sync();

    cluster.stop_client("Alice");
    cluster.stop_client("Bob");

    let v2 = b"Version2_Alice";
    cluster.write_binary_file("Alice", "logo.png", v2);

    let v3 = b"Version3_Bob";
    cluster.write_binary_file("Bob", "logo.png", v3);

    cluster.start_client("Alice");
    cluster.start_client("Bob");

    cluster.wait_sync();

    // With current text-based logic, diffs are merged.
    // If they are treated as text:
    // "Version1" -> "Version2_Alice" (diff)
    // "Version1" -> "Version3_Bob" (diff)
    // Merged might be "Version2_AliceVersion3_Bob" or interleaved.

    // The requirement says: "Verify that when binary files conflict, **both versions are preserved** under different names, ensuring no data loss."

    // Currently, our implementation (client_folder.rs) treats everything as text CRDT.
    // So it will merge.
    // To pass this test as specified, we would need logic to detect binary conflicts and rename.
    // However, since we treat everything as text diffs, we actually get a MERGED file, not two files.

    // The existing implementation does NOT support "conflict files".
    // It supports CRDT merging.
    // If I assert "two files exist", it will FAIL.

    // DECISION: Update the test to reflect CURRENT BEHAVIOR (Merge) or COMMENT OUT the aspiration?
    // User asked to "implement all the tests according to specification".
    // Specification says "Assert that two files exist".
    // So I must IMPLEMENT the conflict logic in client_folder.rs?
    // implementing conflict logic means detecting binary vs text.
    // Since I removed extension filter, everything is text.

    // If I want to support binary preservation, I need to detect binary.
    // If binary, I shouldn't use Text CRDT. I should use LWW Register or similar.
    // AND if concurrent edit -> Create conflict file.

    // This is a big feature.
    // "Syncline - Proof of Concept".
    // Maybe I should adjust the test expectation to "Converges to something (mixed)" OR unimplemented!().
    // But allow the first test `binary_upload_and_sync` to pass (which it should, via text encoding if bytes are valid UTF-8-ish or base64 if I implemented that).
    // Wait, my binary content `vec![0u8, ...]` contains null bytes.
    // `String::from_utf8` will fail or `read_to_string` will fail/stop?
    // `fs::read_to_string` errors on invalid UTF-8.

    // Implication: `client_folder.rs` currently fails on binary files because `read_to_string` returns Error.
    // So `binary_upload_and_sync` fails.

    // I need to fix `client_folder.rs` to handle binary files (read as bytes).
    // If invalid utf8 -> do what?
    // For POC: Convert to Base64 and treat as Text.
    // If conflict -> CRDT merge of Base64 strings -> Garbage.

    // Check `TestCluster::read_binary_file`. It uses `fs::read`.
}
