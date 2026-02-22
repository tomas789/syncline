# Future Work & Technical Debt

This document tracks correctness concerns, design limitations, and technical debt that are not acute bugs but should be addressed before a production release or as the project scales.

---

## Server

### F-1. No Index on `doc_id` Column in SQLite

**Location**: `server/src/db.rs`, `updates` table schema

**Issue**: The `updates` table has no index on `doc_id`. Every `SYNC_STEP_1` request triggers a full sequential scan:

```sql
SELECT update_data FROM updates WHERE doc_id = ? ORDER BY id ASC
```

For a vault with many documents and a long history, this scan grows linearly with the total number of updates across **all** documents, not just the one being requested.

**Recommendation**: Add `CREATE INDEX idx_updates_doc_id ON updates(doc_id, id)`. This is a one-line schema change with no impact on correctness.

### F-2. No Update Compaction / Snapshotting

**Location**: `server/src/db.rs`, `get_all_updates_since`

**Issue**: Every update is appended indefinitely. On each `SYNC_STEP_1`, the server reconstructs the full document state by replaying all historical updates from the beginning of time into a temporary `Doc`. This is `O(total updates)` per connection event and grows without bound as the vault is used.

**Recommendation**: Periodically compact old updates into a single snapshot row per document (storing the full `encode_state_as_update_v1(&StateVector::default())` and deleting the individual rows). This bounds per-connection reconstruction cost to `O(1)` snapshots plus recent deltas.

### F-3. `update_index_for_new_doc` Is Not Fully Atomic

**Location**: `server/src/server.rs`, lines 38–71

**Issue**: The function checks `known_doc_ids` under a write lock, then releases it before locking `index_doc`. In theory, two concurrent callers for the same `doc_id` can both pass the `is_new` guard if they race between the `known_doc_ids.write()` acquire and the `index_doc.lock()`. The `HashSet::insert` return value mitigates this in practice, but the subsequent `index_doc` mutation and DB write are not part of the same atomic section.

**Recommendation**: Either hold the `known_doc_ids` write lock through the entire `index_doc` update, or serialize all index mutations through a dedicated single-writer task.

### F-4. No Authentication or Encryption at the Application Layer

**Location**: `server/src/server.rs`, `ws_handler`

**Issue**: Any client that can reach the server's WebSocket endpoint can read and write all documents. There is no token, session, or identity check. Data is transmitted in plaintext unless a TLS-terminating reverse proxy is placed in front.

**Recommendation**: Add a shared-secret token (passed as a query parameter or HTTP header on WebSocket upgrade) as a minimum access control mechanism. Document the reverse-proxy TLS requirement prominently. End-to-end encryption is tracked separately in `docs/E2EE_IMPLEMENTATION.md`.

---

## Client (Native CLI)

### F-5. Duplicate Diff Logic Between `diff.rs` and `wasm_client.rs`

**Location**: `client_folder/src/diff.rs:4` and `syncline/src/wasm_client.rs:369`

**Issue**: The `apply_diff_to_yrs` function in `diff.rs` and the inline diff loop inside `wasm_client.rs::update()` are functionally identical — both use `dissimilar::diff` and advance a byte-offset cursor. They are maintained separately. A future bug fix or behavioral change in one will not automatically apply to the other.

**Recommendation**: The WASM client should call the shared `apply_diff_to_yrs` function (or the crate should expose a common utility) rather than reimplementing the logic inline.

### F-6. `__index__` Entries Are Never Removed by the CLI Client

**Location**: `client_folder/src/main.rs`, `watcher` event loop

**Issue**: The WASM client exposes `index_remove()` which removes a document from `__index__` when a file is deleted. The CLI client has no equivalent call. As a result, `__index__` grows monotonically and never shrinks, even after files are deleted. The `to_remove` loop in `main.rs:231–250` that would react to `__index__` shrinkage never triggers in a CLI-only deployment.

**Recommendation**: When the CLI client detects a file deletion (watcher or bootstrap), call an equivalent of `index_remove` to delete the doc's entry from `__index__`. This should be coupled with the fix for Bug 13 (actual file deletion propagation).

### F-7. Detached Tasks Have No Lifecycle Bound Under Rapid Reconnection

**Location**: `client_folder/src/network.rs` (see also Bug 14 in `KNOWN_BUGS.md`)

**Issue**: Beyond the immediate task leak described in Bug 14, there is no retry budget or circuit-breaker in the reconnection loop. Under a persistently unavailable server, the client will retry indefinitely with exponential backoff (capped at 30 s). The backoff state is reset on any successful connection, which is correct, but there is no maximum retry count and no way for an operator to observe or limit the reconnection behavior.

**Recommendation**: Add a configurable `--max-retries` flag and/or a Prometheus/OpenTelemetry metric for connection attempts.

### F-8. No Upper Bound Validation on Protocol Fields

**Location**: `syncline/src/protocol.rs`, `decode_message`

**Issue**: The protocol encodes `doc_id_len` as a `u16` (max 65 535 bytes). The decoder correctly bounds-checks the buffer, so there is no memory unsafety. However, there is no validation that `doc_id` is a valid, safe relative path — a malicious or buggy client could send a `doc_id` containing `../` path traversal sequences, which would cause `local_state.root_dir.join(doc_id)` (in `main.rs:280`) to escape the vault directory.

**Recommendation**: Validate that `doc_id` is a clean relative path (no `..` components, no absolute path prefix) both on the server before persisting and on the client before joining with `root_dir`.

### F-9. `doc_id` Uses OS Path Separator

**Location**: `client_folder/src/state.rs`, `get_doc_id`

**Issue**: `doc_id` is derived from `Path::to_string_lossy()`, which uses the OS-native path separator (`\` on Windows). A vault synced between a Windows client and a macOS/Linux client would produce mismatched `doc_id` values for the same file, causing the document to be treated as two separate files.

**Recommendation**: Normalize `doc_id` to use forward slashes (`/`) unconditionally, regardless of OS. Apply `str::replace('\\', '/')` before using the path string as a `doc_id`.

### F-10. `.syncline/data/*.bin` Files for Deleted Documents Are Never Cleaned Up

**Location**: `client_folder/src/storage.rs`, `client_folder/src/state.rs`

**Issue**: When a file is deleted and the deletion is signaled as an empty-content update, the corresponding `.bin` state snapshot is updated to reflect empty content but is never removed from `.syncline/data/`. Over time, the state directory accumulates orphaned `.bin` files for documents that no longer exist.

**Recommendation**: After applying an empty-content update for a deleted document, remove the corresponding `.bin` file from `.syncline/data/`.
