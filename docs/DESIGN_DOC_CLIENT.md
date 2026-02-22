# Syncline Client Design Document

## 1. Executive Summary

This document outlines the architecture, constraints, and implementation phases for the Syncline Client (`client_folder`). The client is a headless Rust daemon that connects to a Syncline server via WebSockets, watching a specific local directory, and ensuring real-time, conflict-free synchronization of `.txt` and `.md` files.

## 2. Architecture & Technology Stack

- **Language**: Rust
- **Async Runtime**: `tokio`
- **File Watching**: `notify` (with `notify-debouncer-mini` or `notify-debouncer-full` to handle bursts).
- **CRDT Engine**: `yrs` (Rust port of Yjs).
- **WebSocket Client**: `tokio-tungstenite`.
- **Logging**: `tracing` and `tracing-subscriber` for extensive, structured logs.
- **Local State Storage**: Stored inside a hidden `.syncline` folder located at the root of the synchronized directory. The state holds the binary encoded Yjs documents to allow for offline change detection.

## 3. Dealing with `Yrs` Limitations

Through investigation of `Yrs` and `Yjs`, we must design around several constraints to guarantee high performance and low memory footprints:

1. **Memory Overhead**: `Yrs` documents keep full history metadata. Simply storing a complete 10MB text requires ~20-40MB of RAM.
2. **Bulk Loading**: A user might sync a directory with 10,000 files. We **cannot** keep 10,000 instances of `yrs::Doc` loaded in memory permanently, as it would cause extreme memory bloat. Instead, docs should be loaded into memory on-demand when handling a file change or incoming network update, and then persisted/dropped when idle.
3. **No Native String Diffing**: If a user modifies a file locally, we only receive the _new raw text_. If we simply clear the Yjs `Text` and insert the new text, the CRDT expands with massive tombstones (deleted characters), destroying performance.
   - **Mitigation**: We must implement an intermediate **String Diffing** step (using a crate like `similar`). When a file changes locally, we compute the _exact_ insertions and deletions between the old version and new version, and apply only those targeted mutations to the `yrs::Text`.

## 4. Handling File System Quirks (OS X Inotify)

The macOS FSEvents (or equivalent) backend for `notify` triggers multiple cascading events per logical user action. For example, editors like Vim or Emacs often perform atomic saves by creating a temporary file and renaming it over the target, generating a burst of `Create`, `Modify`, `Remove`, and `Rename` events.

- **Extensive Logging**: We will introduce generous `tracing::info!` and `tracing::debug!` logs inside the watcher loop to document the exact sequence of events the OS triggers.
- **Observations from tests**: Tests on macOS natively demonstrated that a simple combination of creating `test.md`, writing data, and renaming to `test2.md` emits an overwhelming burst of 13 separate events including `Create`, multiple `Modify(Data)`, `Modify(Metadata)`, and `Modify(Name)` within milliseconds. This confirms that reacting per-event is fundamentally broken on this infrastructure.
- **Debouncing**: We cannot react immediately to an event. We will introduce a tokio-based debounce timer (e.g., 200ms-500ms) keyed by the file path. When a burst of events settles, we read the final file state once.
- **Filter Constraints**: The client will strictly filter and refuse to sync any files not ending in `.md` or `.txt`, as well as entirely ignoring `.syncline/` and `.git/` paths.

## 5. Synchronization & Loop Behavior

1. **Initialization**: On startup, scan the target directory. Compare files on disk against their last known `yrs` state residing in `.syncline/`. Any missing `yrs` states are considered new files; any differences are considered offline local edits.
2. **Server Handshake (`PROTOCOL.md`)**:
   - Open WebSocket to server.
   - Send `MSG_SYNC_STEP_1` (State Vector) for each local document to fetch remote changes.
3. **Processing Remote Updates (`Server ➞ Local`)**:
   - Receive `MSG_UPDATE` or `MSG_SYNC_STEP_2`.
   - Apply to the corresponding `yrs::Doc`.
   - **Crucial Step**: Render the synchronized text and write it to the filesystem. This triggers a local `notify` event. The client MUST uniquely identify its own writes (e.g., by keeping an in-memory hash or a "last written timestamp" check) to avoid sending these back to the server in an infinite loop.
   - **Crucial Step (iCloud / Mobile Placeholder Stubs)**: On mobile platforms like Obsidian for iOS (where files are backed by iCloud Drive), files may initially load as 0-byte (empty `""`) placeholder stubs until fully downloaded. If the synchronization engine processes this empty local stub as user intent during initial sync, it will generate and broadcast a complete document deletion, wiping out the file globally. To prevent this, during the initial file merge phase, the synchronization logic MUST intelligently favor the remote server content instead of the local file if the local content is empty but the server possesses content.
4. **Processing Local Updates (`Local ➞ Server`)**:
   - `notify` debouncer fires for `file.md`.
   - Client reads the raw disk file.
   - Client diffs raw text against the loaded `yrs` text snapshot.
   - Diffs are applied to the `yrs::Doc`.
   - `yrs` generates an update payload, which the client transmits over WebSocket as `MSG_UPDATE`.
   - The updated document binary is saved to `.syncline/`.

## 6. Implementation Phases

We will adhere to an incremental implementation plan, ensuring thorough testing at each step.

### Phase 1: Understanding Inotify & Watcher Infrastructure

- Set up the base `client_folder` crate.
- Implement the `notify` watcher.
- Add comprehensive `tracing` logs.
- Modify files using various editors (VS Code, Vim, TextEdit) on OS X and document the event bursts in tests.
- Create a robust Debouncer module that reliably outputs a single "File Settled" signal.

### Phase 2: Diffing and `Yrs` Integration

- Implement the `similar` diff application logic.
- Write robust unit tests verifying that taking strings A and B precisely transforms a `yrs::Doc` from state A to state B, minimizing insertion/deletion counts.
- Store resulting Yjs updates securely on disk inside an isolated test setting.

### Phase 3: The Local `.syncline` State Store

- Build a local interface to manage documents in the `.syncline/` directory.
- Write mechanisms to load a `.syncline` blob into a `yrs::Doc`, applying local edits, and saving back to a blob.
- Support checking offline differences on start-up.

### Phase 4: Network & WebSocket `PROTOCOL.md` Hookup

- Create the `tokio-tungstenite` connection loop.
- Build the parser and encoder for `MSG_SYNC_STEP_1`, `MSG_SYNC_STEP_2`, and `MSG_UPDATE`.
- Establish syncing for the special `__index__` document.

### Phase 5: Closing the Loop (Conflict resolution & Loop protection)

- Handle remote writes and write them to the local filesystem.
- Introduce "Local File Write Checksums" or timestamps to detect and ignore `notify` events caused by the client itself.

### Phase 6: High Load Testing

- Perform high-load testing: start client in two separate, massive directories and guarantee that `tokio` channels do not overflow. Add backpressure handling if necessary.
