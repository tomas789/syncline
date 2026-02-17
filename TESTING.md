# End-to-End Testing Strategy

This document outlines the comprehensive end-to-end (E2E) testing strategy for the Syncline application. It divides the application into core functional areas and specifies exhaustive test scenarios to ensure robustness, particularly focusing on synchronization logic, offline capabilities, and conflict resolution.

Each test is described as a sequence of actions performed by system actors (Clients, Server) followed by state assertions.

## Functional Areas

1.  **Connectivity & Session Management**: Connection establishment, reconnection, and multi-client presence.
2.  **Real-time Text Synchronization**: Immediate propagation of edits between online clients.
3.  **Offline Reconciliation (Text)**: Handling edits made while disconnected and merging them upon reconnection.
4.  **File System Operations**: Creation, deletion, renaming, and moving of files and directories.
5.  **Binary File Synchronization**: Handling non-text files, including conflict resolution strategies (renaming).
6.  **Persistence & Durability**: ensuring data survives process restarts (Server and Client).

---

## Test Scenarios

### 1. Connectivity & Session Management

#### Basic Connection and Identification

- **Goal**: Verify clients can connect and identify themselves.
- **Source**: [tests/connectivity.rs](tests/connectivity.rs)
- **Steps**:
  1.  Start Server.
  2.  Start Client A (Identity: "Alice").
  3.  Assert Server logs/state indicates "Alice" is connected.
  4.  Start Client B (Identity: "Bob").
  5.  Assert Server logs/state indicates both "Alice" and "Bob" are connected.

#### Reconnection Resilience

- **Goal**: Verify a client can reconnect after a network drop.
- **Source**: [tests/connectivity.rs](tests/connectivity.rs)
- **Steps**:
  1.  Start Server.
  2.  Start Client A. Establish connection.
  3.  Kill Server process.
  4.  Client A should enter "Reconnecting" state (logs/UI).
  5.  Start Server.
  6.  Assert Client A automatically reconnects.

---

### 2. Real-time Text Synchronization

#### Simple Propagation

- **Goal**: Verify text edits propagate immediately between online clients.
- **Source**: [tests/text_sync.rs](tests/text_sync.rs)
- **Steps**:
  1.  Start Server, Client A, Client B.
  2.  Client A creates `note.md` with content "Hello".
  3.  Wait for sync (e.g., 500ms).
  4.  Assert `note.md` exists in Client B's directory with content "Hello".
  5.  Client B appends " World" to `note.md`.
  6.  Wait for sync.
  7.  Assert `note.md` in Client A's directory contains "Hello World".

#### Simultaneous Online Edits

- **Goal**: Verify CRDT usage handles concurrent online edits without data loss.
- **Source**: [tests/text_sync.rs](tests/text_sync.rs)
- **Steps**:
  1.  Start Server, Client A, Client B.
  2.  Ensure `note.md` exists with content "Line 1".
  3.  Client A writes "Client A update" to line 2 _at the same moment_ Client B writes "Client B update" to line 3.
  4.  Wait for sync.
  5.  Assert `note.md` on both clients contains "Line 1" and both updates (order determined by CRDT, but both must exist).

---

### 3. Offline Reconciliation (Text)

#### Single Client Offline Edit

- **Goal**: Verify edits made while offline are synced upon reconnection.
- **Source**: [tests/offline_text.rs](tests/offline_text.rs)
- **Steps**:
  1.  Start Server, Client A, Client B. Sync initial state `doc.txt` = "Base".
  2.  Stop Client A (simulate offline).
  3.  Client A modifies `doc.txt` -> "Base + Offline A".
  4.  Start Client A (reconnect).
  5.  Wait for sync.
  6.  Assert `doc.txt` on Client B is "Base + Offline A".

#### Divergent Offline Edits (The Merge Test)

- **Goal**: Verify the system merges conflicting offline changes using CRDTs.
- **Source**: [tests/offline_text.rs](tests/offline_text.rs)
- **Steps**:
  1.  Start Server, Client A, Client B. Sync initial state `story.md` = "Once upon a time."
  2.  Stop Client A.
  3.  Stop Client B.
  4.  Client A modifies `story.md`: Prepend "Deep in the forest, ".
  5.  Client B modifies `story.md`: Append " The End."
  6.  Start Client A.
  7.  Start Client B.
  8.  Wait for sync/convergence.
  9.  Assert `story.md` on Client A is "Deep in the forest, Once upon a time. The End."
  10. Assert `story.md` on Client B is identically "Deep in the forest, Once upon a time. The End."

#### Offline While Peer Updates

- **Goal**: Verify a client accepts updates that happened while it was offline, merging them with its own local changes.
- **Source**: [tests/offline_text.rs](tests/offline_text.rs)
- **Steps**:
  1.  Start Server, Client A, Client B. Sync initial state.
  2.  Stop Client A.
  3.  Client B creates `new_file.txt`.
  4.  Client A (offline) modifies existing `old_file.txt`.
  5.  Start Client A.
  6.  Assert Client A receives `new_file.txt`.
  7.  Assert Client B receives update to `old_file.txt`.

---

### 4. File System Operations

#### File Deletion Propagation

- **Goal**: Verify deletions are propagated.
- **Source**: [tests/fs_operations.rs](tests/fs_operations.rs)
- **Steps**:
  1.  Start Server, Client A, Client B. Sync initial state (File `delete_me.txt` exists).
  2.  Client A deletes `delete_me.txt`.
  3.  Wait for sync.
  4.  Assert `delete_me.txt` is removed from Client B's directory.

#### File Renaming

- **Goal**: Verify renames are handled (either as move or delete+create, maintaining content).
- **Source**: [tests/fs_operations.rs](tests/fs_operations.rs)
- **Steps**:
  1.  Start Server, Client A, Client B. Sync initial state (File `A.txt` content "Important").
  2.  Client B renames `A.txt` to `B.txt`.
  3.  Wait for sync.
  4.  Assert Client A has `B.txt` with content "Important".
  5.  Assert Client A does NOT have `A.txt`.

#### Directory Creation and Synchronization

- **Goal**: Ensure directory structures are synced.
- **Source**: [tests/fs_operations.rs](tests/fs_operations.rs)
- **Steps**:
  1.  Start Server, Client A, Client B.
  2.  Client A creates directory `Folder/Subfolder`.
  3.  Client A creates `Folder/Subfolder/note.md`.
  4.  Wait for sync.
  5.  Assert Client B has directory `Folder/Subfolder` and contained file.

#### Offline Deletion vs Online Edit (Conflict)

- **Goal**: Verify behavior when one client deletes a file that another edits.
- **Source**: [tests/fs_operations.rs](tests/fs_operations.rs)
- **Steps**:
  1.  Start Server, Client A, Client B. Sync initial File `shared.txt`.
  2.  Stop Client A.
  3.  Client B edits `shared.txt`.
  4.  Client A deletes `shared.txt`.
  5.  Start Client A.
  6.  Wait for sync.
  7.  _Expected Behavior Specification Needed_: Typically, edit wins over delete, or delete wins.
      - Assumption for test: Delete wins (or zombie file recreation).
      - Action: Assert state is consistent on A and B (either both deleted or both present).

---

### 5. Binary File Synchronization

#### Binary Upload and Sync

- **Goal**: Verify binary files (images) sync correctly.
- **Source**: [tests/binary_sync.rs](tests/binary_sync.rs)
- **Steps**:
  1.  Start Server, Client A, Client B.
  2.  Client A copies `image.png` into watch folder.
  3.  Wait for sync.
  4.  Assert Client B has `image.png`.
  5.  Assert SHA256 hash of `image.png` on Client B matches Client A.

#### Binary Conflict (Preservation Strategy)

- **Goal**: Verify that when binary files conflict, **both versions are preserved** under different names, ensuring no data loss.
- **Source**: [tests/binary_sync.rs](tests/binary_sync.rs)
- **Steps**:
  1.  Start Server, Client A, Client B. Sync initial `logo.png` (v1).
  2.  Stop Client A.
  3.  Stop Client B.
  4.  Client A updates `logo.png` (content v2).
  5.  Client B updates `logo.png` (content v3).
  6.  Start Client A and Client B.
  7.  Wait for sync.
  8.  Assert that **two files** exist on both clients (e.g., `logo.png` and `logo (Alice).png`).
  9.  Assert that one file contains content v3 and the other contains content v2.
  10. Assert that **no content was overridden/lost** (both v2 and v3 are accessible).

---

### 6. Persistence & Durability

#### Server Restart

- **Goal**: Verify server retains state across restarts.
- **Source**: [tests/persistence.rs](tests/persistence.rs)
- **Steps**:
  1.  Start Server.
  2.  Start Client A. Create `persistent.txt` = "Data". Wait for sync.
  3.  Stop Client A.
  4.  Stop Server.
  5.  Start Server.
  6.  Start Client B (fresh client, empty dir).
  7.  Wait for sync.
  8.  Assert Client B receives `persistent.txt` = "Data" from Server.

#### Client State Persistence

- **Goal**: Verify client remembers its state (vector clock) to avoid re-syncing everything as new.
- **Source**: [tests/persistence.rs](tests/persistence.rs)
- **Steps**:
  1.  Start Server, Client A. Sync `doc.txt`.
  2.  Stop Client A.
  3.  Modify `doc.txt` on Client A (offline edit).
  4.  Start Client A.
  5.  Assert Client A sends _only_ the new operations (Delta), not the full file (checking logs/network traffic).

#### History Compaction (Long-running Sync)

- **Goal**: Verify server compacts history after many updates, but new clients still receive full state.
- **Source**: [tests/persistence.rs](tests/persistence.rs)
- **Steps**:
  1.  Start Server (configured with low compaction threshold if possible, e.g., 50 updates).
  2.  Start Client A.
  3.  Client A performs 100 small edits to `doc.txt` (triggering compaction on server).
  4.  Start Client B (fresh).
  5.  Wait for sync.
  6.  Assert Client B receives the final state of `doc.txt`.
  7.  _Optional_: Verify server database size is smaller than sum of all updates (if accessible).

---

### 7. Edge Cases & Constraints

#### Permission Errors

- **Goal**: Verify daemon handles file permission errors gracefully (does not crash).
- **Source**: [tests/edge_cases.rs](tests/edge_cases.rs)
- **Steps**:
  1.  Start Server, Client A.
  2.  Create `locked.txt` on Client A. Sync to Server.
  3.  On Client A, `chmod 000 locked.txt` (remove read/write perms).
  4.  Server receives update for `locked.txt` from another client (e.g., Client B).
  5.  Client A attempts to apply update.
  6.  Assert Client A logs error but stays running.
  7.  Restore permissions on `locked.txt`.
  8.  Assert Client A eventually converges (retries or next sync cycle).

#### Large File Handling

- **Goal**: Verify system handles files larger than typical WebSocket frame limits.
- **Source**: [tests/edge_cases.rs](tests/edge_cases.rs)
- **Steps**:
  1.  Start Server, Client A, Client B.
  2.  Client A creates a 50MB text/binary file.
  3.  Wait for sync.
  4.  Assert Client B receives the file with matching checksum.
