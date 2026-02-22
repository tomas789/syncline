This is a high-level architectural design document (ADD) for a custom, robust synchronization system for Obsidian, focusing on conflict-free merging, offline capabilities, and cross-platform support (Obsidian Plugin \+ Linux/Windows Daemon).

### **1\. Executive Summary**

To achieve "rock-solid" sync without requiring user manual merging, we will utilize **Conflict-free Replicated Data Types (CRDTs)**. Specifically, we will use the **Yjs** ecosystem.

- **Why Yjs?** It is the industry standard for decentralized text synchronization. It handles the "A adds to top, B adds to bottom" scenario natively.
- **Interoperability:** It has a robust TypeScript library (yjs) for the Obsidian plugin and a high-performance Rust port (yrs) for the Server and Linux Daemon, allowing them to share the exact same logic and binary encoding.

### **2\. Technology Stack**

- **Shared Core (The "Brain"):** yrs (Rust) compiled to Wasm for the Plugin, and native for the Daemon/Server. This ensures identical sync logic across all nodes.
- **Server:** Rust \+ Axum (WebSockets) \+ SQLx.
- **Database:** SQLite (Embedded, single-file, easy to backup).
- **Obsidian Plugin:** TypeScript \+ y-websocket (customized) \+ y-indexeddb (for local persistence).
- **Linux/Windows Daemon:** Rust \+ notify (file watching) \+ tokio.

### ---

**3\. Architecture Overview**

The system follows a **Star Topology** with a "Smart Server" approach. The server is the central authority for ordering messages and storing durability, but the logic for merging text resides in the shared CRDT algorithm.

#### **3.1 The Data Model**

We will treat the Vault not as a file system, but as a collection of **Documents**.

- **Document ID:** A UUID assigned to every file. We do _not_ rely on filenames as IDs (to support renaming).
- **Map Structure:** A global y-map stores the directory tree: UUID \-\> { Metadata, Filename, ParentID }.
- **Text Files:** Stored as y-text updates (deltas).
- **Binary Files:** Stored as BLOBs with Last-Write-Wins (LWW) \+ Conflict Renaming.

### ---

**4\. Detailed Component Design**

#### **4.1 The Server (Rust/Axum)**

The server is a "dumb pipe" for real-time messages but a "smart store" for persistence.

- **WebSocket Endpoint:** Accepts binary streams.
- **Persistence:** It does not reconstruct the file tree constantly. Instead, it appends binary CRDT updates to SQLite.
- **Compaction Engine:** A background thread that reads the log of updates, merges them into a single "Snapshot" blob, and deletes the individual history logs to save space.

**SQLite Schema:**

SQL

\-- Tracks connected clients and their last known sync state  
CREATE TABLE clients (  
 client_id TEXT PRIMARY KEY,  
 last_seen TIMESTAMP,  
 vector_clock BLOB  
);

\-- Stores the raw CRDT updates  
CREATE TABLE updates (  
 doc_id TEXT,  
 update_data BLOB, \-- The binary Yjs update  
 created_at TIMESTAMP  
);

\-- Stores "Squashed" snapshots (Compaction)  
CREATE TABLE snapshots (  
 doc_id TEXT PRIMARY KEY,  
 snapshot_data BLOB  
);

\-- Binary file storage  
CREATE TABLE blobs (  
 sha256_hash TEXT PRIMARY KEY,  
 data BLOB  
);

#### **4.2 The Obsidian Plugin (TypeScript)**

- **Intercept:** Hooks into vault.on('modify').
- **Diffing:** When a user types, the plugin calculates the diff and applies it to the local Yjs document.
- **Sync:** The Yjs document emits a binary "update". This is pushed to the WebSocket.
- **Incoming:** When a message arrives from WebSocket, apply it to the Yjs doc, then write the result to the Obsidian file system.

#### **4.3 The Linux/Windows Daemon (Rust)**

- **Watcher:** Uses notify crate to watch the target directory recursively.
- **Debounce:** Essential to prevent infinite loops (Server updates file \-\> Watcher sees change \-\> Watcher sends update \-\> Server sends back...). The daemon must ignore file events triggered by its own write operations.
- **Headless:** Runs in the background (systemd service).

### ---

**5\. Solving the Core Scenarios**

#### **Scenario 1: The "A and B Offline" Case (Text)**

- **Initial State:** Text is "Hello World".
- **Offline A:** Edits top: "**Big** Hello World". (Yjs creates a cached insert operation at index 0).
- **Offline B:** Edits bottom: "Hello World **again**". (Yjs creates a cached insert operation at index 11).
- **Reconnection:**
  1. A connects. Sends update vector. Server appends.
  2. B connects. Sends update vector. Server appends.
  3. Server broadcasts A's update to B, and B's to A.
- **Result:** Yjs mathematically guarantees convergence. Both clients result in "**Big** Hello World **again**". No manual merge required.

#### **Scenario 2: Binary Files & Conflicts**

CRDTs are bad for images/PDFs. We use a **Content-Addressable** approach.

1. **Upload:** When Client A saves image.png, it calculates the SHA256 hash. It uploads the blob to the server if the server doesn't have that hash.
2. **Metadata:** Client A updates the file tree map: FileUUID \-\> { Hash: "abc...", Timestamp: 100 }.
3. **Conflict:**
   - Client B is offline and changes image.png (Hash "xyz...").
   - Client A changes image.png (Hash "123...").
   - Both sync.
4. **Resolution:** The system detects concurrent modification on the same File UUID.
   - **Winner:** The update with the later server-time timestamp keeps the name image.png.
   - **Loser:** The other hash is _not_ discarded. The system automatically creates a new file entry: image.png_conflict_hostnameB.

### ---

**6\. Lifecycle Management: Snapshots & Compaction**

To prevent SQLite from growing indefinitely, we implement **Lazy Compaction**.

1. **The Problem:** Storing every single keystroke (delta) forever is inefficient.
2. **The Solution:**
   - The Server monitors the size of updates table for a specific doc_id.
   - Once it hits a threshold (e.g., 50 updates), the server loads the current Snapshot \+ all 50 updates into memory (using yrs), merges them, and saves a **New Snapshot**.
   - It deletes the 50 old updates.
3. **New Client Edge Case:**
   - If a brand new client joins, they don't need the history. The server just sends the latest Snapshot.
   - If an _old_ client (offline for months) joins and asks for updates that have been deleted (compacted), the server responds with: ERR_HISTORY_LOST.
   - **Recovery:** The client must nuke its local metadata and accept the full Server Snapshot (Fresh Start).

### ---

**7\. Edge Case Analysis**

| Edge Case                     | Solution                                                                                                                                                          |
| :---------------------------- | :---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **System Crash during Write** | SQLite transactions ensure atomicity. If power fails while writing a sync update, it rolls back.                                                                  |
| **Rename Loop**               | A renames X \-\> Y. B renames Y \-\> X. Timestamps determine the final state. If timestamps are identical (rare), sort by Client ID.                              |
| **Clock Skew**                | We do not rely on Client System Time for ordering critical events. We use **Vector Clocks** (logical time) for CRDTs and Server Time for binary LWW.              |
| **Partial Sync**              | Client A loses internet halfway through sending a large PDF. The server uses a temporary upload staging area. The commit only happens when the full hash matches. |
| **Permissions (Linux)**       | The Rust daemon must run as the user who owns the files (or use setgid) to avoid permission errors when writing files synced from other machines.                 |

### **8\. Implementation Roadmap**

1. **Phase 1 (The Core):** Implement the Server (Axum) and a CLI Rust client that can sync a single text file using yrs.
2. **Phase 2 (The Plugin):** Build the Obsidian plugin UI to input Server URL and connect via WebSocket. Implement the basic vault.on('modify') hook.
3. **Phase 3 (The Daemon):** extend the CLI client to watch a folder recursively.
4. **Phase 4 (Binaries):** Implement the Blob upload/download and the conflict renaming logic.

Would you like me to generate the specific **Rust Struct definitions** for the Protocol messages or the **SQL schema** for the server?

---

## Original prompt

In obsidian, you can sync using their own sync or using live-sync. The other option is quite unreliable. Let's say I'd like to implement my own sync method that would be rock solid and would support syncing to linux directory. Create an architectural desing document for that. There should be an Obsidian plugin and linux package. It needs to work reasonably well even when multiple clients are offline and then reconnect. I don't want users to need merging.

It needs to cover situations like: A and B are fully in sync with server. They both go offline, A adds paragraph at the beginning, B and the end, They both go online, A and B get both new paragraphs.

Architecturally, there must be a daemon/server that can have whatever state. Then, there must be a Obsidian plugin and Linux/Windows client that can synchronize same as Obsidian but without running Obsidian.

It must also be able to handle binary files like images and PDFs in the vault.

We should be using Rust where reasonable and Typescript for obsidian. The server should be websocket based with Axum/Rust. Server should be using SQLite for storage.

Include edge-case analysis.

Include snapshots for new clients and "compaction" where it drops really old changes such that the SQLite does not grow indefinitely.

When conflicts appear in binary files, I want to distinguish them by differnet names (like filename plus hostname
