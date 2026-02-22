# Known Bugs and Code Smells

This document outlines the known bugs, performance bottlenecks, and code smells in the Syncline codebase (excluding the Obsidian plugin logic).

**Note**: Unit tests for issues 1, 2, 3, 5, 6, 11 have been created or fixed to verify these bugs and prevent regressions. These bugs are now fixed.

## Server (`server/src/*`)

### 1. CRITICAL BUG: Massive Task & Channel Leak on Websocket Reconnections (FIXED)

**Location**: `server/src/server.rs`, `MSG_SYNC_STEP_1` handler.

**Issue**: When a client requests synchronization, the server spins up a `tokio::spawn` task to wait for incoming broadcast messages (`rx.recv().await`) and forward them via the websocket (`tx_fwd.send(msg)`). However, if the client disconnects and nobody modifies the document, the task stays stuck on `rx.recv().await` forever. Over time, as clients reconnect or drop out, the server will leak `tokio` tasks and accumulate ghost subscriptions taking up memory limits.

**Proposed Fix**: Break the loop aggressively by wrapping the receive in a `tokio::select!` block watching if the forwarder websocket was disconnected.

```rust
loop {
    tokio::select! {
        _ = tx_fwd.closed() => break, // Task cleanly exits if client drops
        res = rx.recv() => {
            if let Ok(payload) = res {
                let msg = encode_message(MSG_UPDATE, &doc_id_str, &payload);
                if tx_fwd.send(msg).is_err() { break; }
            } else if let Err(broadcast::error::RecvError::Closed) = res {
                break;
            }
        }
    }
}
```

### 2. BUG: Re-Echoing Updates Back to Origin Sender (FIXED)

**Location**: `server/src/server.rs`, `MSG_UPDATE` handler.

**Issue**: Upon receiving `MSG_UPDATE`, the server broadcasts the raw bytes directly to all `channel` listeners. However, the client that sent this message is also subscribed to that same broadcast. The websocket ends up returning the exact same update back to the sender.

**Impact**: `yjs` handles duplicate CRDT updates fine, but re-echoing massive documents wastes immediate bandwidth and server overhead on duplicate transmissions.

### 3. CRITICAL BUG: Updates for New Documents Silently Dropped — Clients Never Converge (FIXED)

**Location**: `client_folder/src/main.rs` (watcher handler, lines 137–187) and `server/src/server.rs` (`MSG_UPDATE` handler, lines 148–163).

**Issue**: When the file watcher discovers a newly created file, the client sends `MSG_UPDATE` directly without first sending `MSG_SYNC_STEP_1` for that document. The server only creates per-document broadcast channels in response to `MSG_SYNC_STEP_1` (line 79–87). In the `MSG_UPDATE` handler, `channels.get(doc_id)` returns `None` for any document that no client has subscribed to, so the update is saved to SQLite but **never relayed** to other connected clients.

```rust
// server MSG_UPDATE handler — silently drops if no channel exists
let channels = state_clone.channels.read().await;
if let Some(tx) = channels.get(doc_id) {   // None for new docs!
    let _ = tx.send((payload.to_vec(), connection_id));
}
```

**Impact**: Every client operates in complete isolation for any file created after the initial connection handshake. The fuzzer (`cargo run --bin fuzzer -- --duration-secs 5`) reproduces this reliably: all three clients diverge on every file because none of their updates are ever broadcast.

**Regression test**: `server::tests::test_updates_for_new_docs_are_relayed_between_clients` — asserts that Client A receives Client B's update for a document neither has `SyncStep1`'d; currently fails.

**Proposed Fix** (both sides):

1. **Client**: Before sending `MSG_UPDATE` for a document that hasn't been synced yet, send `MSG_SYNC_STEP_1` with the document's current state vector. This creates the broadcast channel on the server and subscribes the client to it.
2. **Server**: Auto-create the broadcast channel when receiving `MSG_UPDATE` for an unknown `doc_id`, so that late-subscribing clients don't miss updates due to ordering races.

### 4. PERFORMANCE SMELL: `O(N)` Blocking Database Calls during Connection Events (FIXED)

**Location**: `server/src/db.rs` (`get_all_updates_since`) and `server/src/server.rs` (`recv_task`).

**Issue**: On every synchronization request (`SYNC_STEP_1`), the server loops over _all historical updates_ from SQLite, unpacks them sequentially into an ephemeral `Doc()`, computes the differences, and returns the response. Crucially, this is `await`ed inline inside `ws_handler`, locking the websocket event loop while pulling strings out of the database array.

**Impact**: Because it happens directly in the sequential payload loop, clients with 50 notes updating concurrently will severely choke the initial WebSocket loading step.

**Proposed Fix**: Consider wrapping heavy `db.get_all_updates_since` instances in `tokio::task::spawn_blocking` to preserve executor threads, or ideally, add database-level compaction (`yjs` snapshots) long term so the server isn't rebuilding a CRDT struct from epoch 0.

### 5. BUG: Unicode Corruption & Encoding Index Skew on New Documents in `__index__` (FIXED)

**Location**: `server/src/server.rs`, `update_index_for_new_doc`.

**Issue**: The code appending a new `doc_id` to the global `__index__` uses `.chars().count()` for the Yjs index map:

```rust
let current = index_text.get_string(&txn);
let len = current.chars().count() as u32;
index_text.insert(&mut txn, len, &format!("{}\n", doc_id));
```

Since `yrs` 0.17+ structurally evaluates insertion indices via UTF-8 byte lengths, counting scalar Unicode characters will completely de-sync the CRDT map the moment a filename with multi-byte unicode characters (e.g. Emoji, non-ascii characters) is successfully registered. This mirrors the previously fixed issue #5 in the client diffing utility.

**Proposed Fix**: Switch `.chars().count()` to `.len()` for raw byte lengths.

## Client Native Folder (`client_folder/src/*`)

### 5. BUG: Unicode Corruption & Encoding Index Skew (FIXED)

**Location**: `client_folder/src/diff.rs`, `apply_diff_to_yrs()`.

**Issue**: The diff iteration increments the tracking cursor utilizing `.chars().count()` for `yrs` insertion indices:

```rust
let len = change.value().chars().count();
cursor += len as u32;
text_ref.remove_range(&mut txn, cursor, len as u32);
```

Since `yrs` `0.17+` moved offset counts structurally in pure Rust contexts to function against underlying UTF-8 lengths (via `OffsetKind::Bytes`), counting scalar Unicode values instead of bytes will completely de-sync the index map the second a user types a multi-byte character like an Emoji (`🚀`) or special diacritic notation.

**Proposed Fix**: Switch `.chars().count()` to raw byte slices `.len()` incrementing throughout `apply_diff_to_yrs` to stay natively safe.

### 6. BUG: Premature Loop Interruptions During Offline Sync (FIXED)

**Location**: `client_folder/src/state.rs`, `bootstrap_offline_changes`.

**Issue**: Using the Early Return operator (`?`) within the iterator causes severe fallout:

```rust
let doc_id = self.get_doc_id(path)?; // Returns an Err() cancelling everything
```

If just a highly constrained user-permission blocks a hidden `.md` file, the `?` halts the entire global search loop, throwing an error and entirely wiping out sync checks for the remainder of the directory tree.

**Proposed Fix**: Handle isolated read paths using nested `match` scopes locally (logging an error) inside the `for` loop and executing a smooth `continue;` statement.

### 7. CODE SMELL: Overly Aggressive Filtering (FIXED)

**Location**: `client_folder/src/state.rs`, `bootstrap_offline_changes`.

**Issue**: The path exclusion code filters heavily based on prefixes:

```rust
!name.starts_with(".git") && !name.starts_with(".syncline")
```

While this stops recursing into the `.git/` folder, it will actively drop syncing for totally viable plaintext or markdown files named `.gitignore`, `.github/action.yml`, `.git_cheatsheet.md`, etc.

**Proposed Fix**: Instead of generic string prefixes, evaluate against strictly `name == ".git"` and `name == ".syncline"` string matches for directory paths.

### 8. CODE SMELL: FSEvents/inotify OS Dispatch Block (FIXED)

**Location**: `client_folder/src/watcher.rs`, `tx.blocking_send(event)`.

**Issue**: Pushing new events utilizes a `blocking_send` to the `tokio` bounded buffer inside the unmanaged system-level OS native thread that `notify` allocates. If an operating system creates thousands of file change event cascades (like installing a local package inside the tree structure), it blocks the callback thread.

**Proposed Fix**: Change channels towards unbounded capacities or use asynchronous `try_send` logic allowing dropping and reporting if the system falls too far behind the executor loop.

### 9. CRITICAL BUG / DATA LOSS: Ignored Local Deletions & Renamed Files Leaking (FIXED)

**Location**: `client_folder/src/main.rs` & `client_folder/src/state.rs`

**Issue**: The client completely ignores all file deletion side-effects. Within `main.rs`, the `notify` loop drops file events that no longer resolve to an active physical file. Within `state.rs`, `bootstrap_offline_changes` traverses only physical files through `WalkDir`, skipping any orphaned `.bin` states. Resultingly, if a user deletes or renames a file (a Rename is practically a Delete + Create) locally, the server isn't notified and `.syncline/` silently retains the original document context. Any subsequent server restart, connection refresh, or remote peer update to this file instantly resurrects the deleted file back onto the physical disk.

**Proposed Fix**: `bootstrap_offline_changes` should iteratively diff the directory traversal bounds with the complete list of known `doc_id` inside `.syncline/data/`. `notify` handlers need to intercept delete events and explicitly encode an empty string push upon deletion.

### 10. BUG / DATA LOSS: Unhandled Read Errors Formulate Wipeout Commits (FIXED)

**Location**: `client_folder/src/main.rs`, inside remote update diffing section.

**Issue**: Prior to merging remote commits, the client reads the disk file to ascertain any interrupted local edits:

```rust
let has_file = fs::metadata(&phys_path).is_ok();
let disk_content = fs::read_to_string(&phys_path).unwrap_or_default();
```

If a file exists (`metadata().is_ok()`) but an OS transient lock or permission restriction returns a read error, `unwrap_or_default()` supplies an empty string `""`. The application subsequently computes a diff from the complete CRDT string traversing to this empty string, deleting the entire document contents, and propagates the permanent text wipeout globally back to the server.

**Proposed Fix**: Remove `.unwrap_or_default()` and propagate or correctly pattern-match local filesystem `io::Error`s, purposefully skipping the synchronization step if the local disk read operation aborts.

### 11. PERFORMANCE BUG: Remote Sync Requests Cascade Unconditional `fs::write` Overwrites (FIXED)

**Location**: `client_folder/src/main.rs`, `MSG_UPDATE` handling block.

**Issue**: Upon receiving `SyncStep2` or `MSG_UPDATE`, decoding the CRDT blindly results in a raw `fs::write(&phys_path, text_val)`, with zero logic verifying if the local physical contents actually changed contextually.
Furthermore, the server's `db.get_all_updates_since()` inherently broadcasts a 2-byte default SV payload (`[0,0]`) for an up-to-date document instead of an empty vector. Upon connection startup, this executes an update for _every_ individual file! The client overrides every single file across the workspace verbatim with the same text, triggering aggressive filesystem `mtime` updates and thousands of redundant destructive `fsevents` looping the filesystem watchers.

**Proposed Fix**: Only invoke `fs::write` on physical files if `text_val != disk_content`.

### 12. PERFORMANCE SMELL: Heavy Offline Initial Updates Broadcast Complete Network Histories (FIXED)

**Location**: `client_folder/src/main.rs`, `bootstrap_offline_changes` reporting

**Issue**: For any file modified while offline, the local client broadcasts local changes utilizing:

```rust
let full_update = doc.transact().encode_state_as_update_v1(&StateVector::default());
```

Comparing relative to `&StateVector::default()` packages the _entire exact keystroke log_ comprising uncompacted historic inserts instead of formulating a precise diff spanning across from the baseline `.syncline` backup to the state modified currently inside the CRDT. This results in wasting considerable `O(N)` bandwidth scaling linearly per file size.

**Proposed Fix**: Construct the string representation delta explicitly relative to the historical backup baseline (`&previous_sv`) to yield incremental binary sizes.
