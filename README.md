# Syncline - Proof of Concept

This is a robust, minimal proof of concept for the Syncline synchronization system.

## Core Technology

- **Yrs (Yjs Rust port)**: Handles CRDT logic for conflict-free text synchronization.
- **Axum**: High-performance Rust web server for WebSockets.
- **SQLite (SQLx)**: Persists updates locally on the server.
- **Tokio**: Asynchronous runtime.

## Verification

The core technology is verified using a comprehensive integration test that simulates a full synchronization cycle, including:

[Detailed Testing Strategy](TESTING.md)

1.  **Server Startup**: Initializes SQLite (in-memory) and starts WebSocket server.
2.  **Client Connection**: Two independent clients (A and B) connect.
3.  **Real-time Sync**: Client A edits, Client B receives.
4.  **Concurrent Edits (Simulated Offline)**: Both clients make changes simultaneously without syncing.
5.  **Convergence**: Both clients push updates, and the system resolves them to the correct state ("Big Hello World").

## Running the Verification

To run the proof of concept verification test:

```bash
cargo test --test integration_tests
```

## Running the Binaries

You can also run the independent Server and Client binaries to test manually.

### 1. Start the Server

```bash
cargo run --bin server
# Default port: 3030
# Default DB: syncline.db (persisted file)
# Options: cargo run --bin server -- --port 4000 --db-path myStart.db
```

### 2. Start Client A

Open a new terminal:

```bash
cargo run --bin client -- --name Alice
```

### 3. Start Client B

Open another terminal:

```bash
cargo run --bin client -- --name Bob
```

### 4. Test Sync

- Type "Hello from Alice" in Alice's terminal.
- See it appear in Bob's terminal.
- Type "Hi Alice" in Bob's terminal.
- See it appear in Alice's terminal.

### 5. Folder Sync (Directory Daemon)

The `client_folder` binary watches a directory for file changes and synchronizes `.md` and `.txt` files across clients using the server.

```bash
# Start the server first
cargo run --bin server

# Client A: sync ~/obsidian-vault
cargo run --bin client_folder -- --dir ~/obsidian-vault

# Client B: sync ~/obsidian-vault-copy (on another machine or terminal)
cargo run --bin client_folder -- --url ws://server-ip:3030 --dir ~/obsidian-vault-copy
```

Changes made to files in either directory will automatically propagate to the other. Concurrent edits are merged using CRDTs.

### 6. Running All Tests

```bash
# Run all tests (integration + E2E folder sync)
cargo test

# Run only the folder sync E2E test (with logs)
RUST_LOG=info cargo test --test folder_sync_test -- --nocapture

# Run only the core integration test
cargo test --test integration_tests
```

### 7. TODOs

- [ ] Ensure the core library can be cross-compiled to WASM such that it can be used in the Obsidian plugin.
- [ ] Create CI/CD pipeline for the Github Actions.
- [ ] Create a simple web UI for the daemon.
- [x] Give the client support for reading from real files and synchronizing the whole directory.
- [ ] Create the Obsidian plugin.
- [x] Add support for binary files.
- [ ] Make sure it supports Windows, Linux and macOS. Mainly the inotify could be an issue.
- [ ] Figure out how to do the database migrations.
- [ ] Snapshot-based initial synchronization.
- [ ] Benchmark how the database size (both server and client state) grows with number of files / edits of a file.
- [ ] Benchmark how many clients can be connected to the server.
