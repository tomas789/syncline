# Syncline Protocol Documentation

Syncline uses a custom binary protocol over WebSockets for bidirectional real-time synchronization. It relies on the [Yjs](https://yjs.dev/) framework (and its Rust port, `yrs`) to handle Conflict-free Replicated Data Types (CRDTs).

This document outlines everything necessary to implement a client that communicates with the Syncline server.

## Connection

The client must connect to the server via WebSocket:

```
ws://<server-host>:<port>/sync
```

The WebSocket must be configured to process binary data. In Javascript/Typescript, this means setting:

```javascript
websocket.binaryType = "arraybuffer";
```

## Binary Message Format

Every message exchanged between the client and the server has the following binary structure:

| Field        | Size               | Details                                                                              |
| ------------ | ------------------ | ------------------------------------------------------------------------------------ |
| `msg_type`   | 1 byte             | The type of the message (see below).                                                 |
| `doc_id_len` | 2 bytes            | The length of the `doc_id` string, encoded as an unsigned 16-bit big-endian integer. |
| `doc_id`     | `doc_id_len` bytes | The Document ID encoded as a UTF-8 string.                                           |
| `payload`    | Remaining bytes    | The message payload, typically a Yjs standard v1 encoded State Vector or Update.     |

### Message Types

There are three types of messages defined in the protocol:

- **`MSG_SYNC_STEP_1` (0x00)**
  - **Direction**: Client ➞ Server
  - **Purpose**: Sent by the client to request missing updates from the server and subscribe to future updates for this document.
  - **Payload**: The client's local Yjs Document encoded as a State Vector (`Y.encodeStateVector(doc)`).
  - **Side effect**: The server creates a broadcast channel for this `doc_id` (if one does not already exist) and subscribes the client to it. If the `doc_id` is not `"__index__"`, the server also registers it in the `__index__` document.

- **`MSG_SYNC_STEP_2` (0x01)**
  - **Direction**: Server ➞ Client
  - **Purpose**: Sent by the server in response to `MSG_SYNC_STEP_1`, containing any updates the client is missing.
  - **Payload**: A Yjs Document update (`Y.encodeStateAsUpdate(doc, stateVector)` equivalent).

- **`MSG_UPDATE` (0x02)**
  - **Direction**: Bidirectional (Client ➞ Server, Server ➞ Client)
  - **Purpose**: Disseminates newly applied changes to the document.
  - **Payload**: A Yjs Document update.
  - **Server behavior**: When the server receives a `MSG_UPDATE`, it (1) registers the `doc_id` in `__index__` if new, (2) persists the update to the database, and (3) broadcasts the update to all subscribers of the document's broadcast channel **except** the sender.

## Document Identification

Syncline uses **UUIDs** as document identifiers. Each file in the vault is assigned a unique UUID when it is first created or discovered. The UUID is stable across renames — renaming a file changes its metadata, not its UUID.

The mapping between a file's path and its UUID is maintained through two mechanisms:

1. **`meta.path`** — a CRDT field inside each document (see [File Documents](#file-documents) below) that stores the canonical file path. This is the source of truth that all clients use to determine where a file should exist on disk.
2. **Client-local path map** — each client maintains a local lookup table (e.g., `.syncline/data/path_map.json` in the CLI client) mapping `relative_path → UUID` for fast lookups. This is not synced and is rebuilt from `meta.path` fields as needed.

## Client Implementation Guidelines

To correctly synchronize documents with the server, a client should follow this lifecycle.

### 1. Initialization and Connection

- Maintain an individual Yjs Document (`Y.Doc`) for each `doc_id` you want to sync.
- Wait for the WebSocket connection to establish (`onopen` event).

### 2. Initial Synchronization

Once connected, the client should synchronize its local state with the server:

1. **Subscribe to `__index__`**: Send a `MSG_SYNC_STEP_1` for `doc_id = "__index__"` with an empty (or local) State Vector. The server will respond with a `MSG_SYNC_STEP_2` containing the full index.

2. **Subscribe to known documents**: For each UUID the client already knows about (e.g., from persisted `.bin` state files), send a `MSG_SYNC_STEP_1` with the local document's State Vector.

3. **Broadcast offline changes** _(first connection only)_: If the client accumulated changes while offline, send them as `MSG_UPDATE` messages for each affected UUID. This ensures the server integrates any offline edits.

### 3. Discovering New Documents

When the client receives an update for `__index__`, it should:

1. Parse the index content (newline-separated UUIDs).
2. Identify any UUIDs not already tracked locally.
3. For each newly discovered UUID, send a `MSG_SYNC_STEP_1` to subscribe and receive the document's content.

When then client receives the new document's content via `MSG_SYNC_STEP_2`, it should:

1. Read the `meta.path` value from the document's `meta` Y.Map.
2. Write the document's text content to the corresponding file path on disk.

### 4. Handling Remote Updates

The client needs to listen for incoming WebSocket messages (`onmessage`).

- Parse the incoming binary buffer into `msg_type`, `doc_id`, and `payload`.
- If the `doc_id` matches a locally tracked document:
  - If the `msg_type` is `MSG_SYNC_STEP_2` or `MSG_UPDATE`:
    - Apply the `payload` to the local Yjs Document using `Y.applyUpdate(doc, payload)`.
    - Read `meta.path` from the document — if it has changed, handle the rename (see [Rename Propagation](#rename-propagation)).
    - Write the updated content to disk.
    - _Note: You must ensure you do not re-broadcast this applied update back to the server (e.g., pause local update observers or check flags before transmitting)._

### 5. Broadcasting Local Changes

When the local document is modified by the user (or the application layer):

- The Yjs Document will emit an `update` event (`doc.on('update', (update, origin) => { ... })`).
- If the update originated locally (not from the WebSocket), dispatch a `MSG_UPDATE` with the event's `update` buffer to the server.

### 6. Creating New Files

When a new file is created locally:

1. Generate a new **UUID** for the file.
2. Create a new Y.Doc. Set `meta.path` to the relative file path. Set `content` to the file's text.
3. Send `MSG_SYNC_STEP_1` for the new UUID to subscribe to its broadcast channel.
4. Send `MSG_UPDATE` with the document's full state.
5. Insert the UUID into the `__index__` document and broadcast the index update.

### 7. Deleting Files

When a file is deleted locally:

1. Clear the document's `content` text (set to empty string via CRDT operations).
2. Remove the UUID from the `__index__` document.
3. Broadcast both updates.
4. Remote clients receiving the index update will detect the UUID removal and delete the corresponding local file.

## Schema & Special Documents

Syncline defines standard structures for its documents over Yjs.

### The Index Document (`__index__`)

The server and clients use a special reserved document ID `"__index__"` to track the list of all synchronized files in the workspace.

- **`doc_id`**: `"__index__"`
- **Schema**: Contains a single Yjs Text named `"content"` (`doc.getText('content')` / `doc.get_or_insert_text("content")`).
- **Data format**: A plain text string containing one UUID per line (newline-terminated). For example:

  ```
  a3b8d1b6-0b3b-4b1a-9c1a-1a2b3c4d5e6f
  f81d4fae-7dec-11d0-a765-00a0c91e6bf6
  4192bff0-e1e0-43ce-a4db-912808c32493
  ```

- **Operations**:
  - **Insert**: Append `"{uuid}\n"` at the end of the text.
  - **Remove**: Find and delete the `"{uuid}\n"` substring from the text.
  - **List**: Split the text by newlines and filter out empty strings.

- **Server behavior**: The server automatically registers new `doc_id`s in the index when it receives a `MSG_SYNC_STEP_1` or `MSG_UPDATE` for a UUID that it hasn't seen before. The server maintains a `known_doc_ids` set in memory (rebuilt from the index on startup) to ensure each UUID is inserted exactly once.

### File Documents

Each file in the vault is represented by a Yjs Document identified by a **UUID**.

The document contains two top-level Yjs structures:

| Yrs Type | Name        | Purpose                                                      |
| -------- | ----------- | ------------------------------------------------------------ |
| `Y.Text` | `"content"` | The file's text content.                                     |
| `Y.Map`  | `"meta"`    | Metadata about the file, currently containing the file path. |

#### The `content` Text

- **Type**: `doc.getText('content')` / `doc.get_or_insert_text("content")`
- **Data**: The entire text content of the file. Syncing this Yjs Text guarantees real-time collaborative text editing with conflict-free merging.

#### The `meta` Map

- **Type**: `doc.getMap('meta')` / `doc.get_or_insert_map("meta")`
- **Keys**:
  - `"path"` (`string`) — The relative file path within the vault (e.g., `"notes/idea.md"` or `"journal/2024-01-15.md"`).

The `meta.path` field is critical for:

- **File location**: Clients use `meta.path` to determine where to write the file on disk.
- **Rename propagation**: When a file is renamed, only `meta.path` is updated. The UUID stays the same, so the CRDT history is preserved.
- **Path conflict resolution**: When two clients independently create files at the same path, they will have different UUIDs. The conflict is detected and resolved using `meta.path`.

## Rename Propagation

Renames are propagated through the CRDT by updating the `meta.path` field:

1. The renaming client updates `meta.path` in the document's `meta` Y.Map to the new path.
2. This generates a CRDT update that is broadcast to all subscribers.
3. Remote clients receive the update, read the new `meta.path` value, detect that it differs from the previously known path, and rename the file on disk.

Because the UUID remains constant, the full edit history and CRDT state are preserved across renames.

### Client-specific rename detection

The two client types detect renames differently:

- **Obsidian plugin**: Receives atomic `vault.on('rename')` events from the Obsidian API.
- **CLI/Folder client**: Uses content-hash matching across batched file-watcher events. When a delete and a create appear in the same event batch with matching content, the client treats it as a rename rather than a delete-then-create.

## Path Conflict Resolution

When two clients independently create files at the same path while offline, they will generate different UUIDs for the same path. Upon reconnection, a path collision is detected:

1. The server's UUID (the one that was in the `__index__` before the client connected) is treated as canonical.
2. The locally-created UUID's file is renamed to a conflict filename: `"notes/idea (client-name).md"`.
3. Both documents are synced to the server. The user can manually reconcile the conflict.
