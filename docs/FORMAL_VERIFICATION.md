# Formal Verification of the Syncline Synchronization Protocol

This document describes how to formally verify the Syncline synchronization protocol using [TLA+](https://lamport.azurewebsites.net/tla/tla.html) — a formal specification language designed by Leslie Lamport for modeling concurrent and distributed systems.

## 1. Why Formal Verification?

Syncline's synchronization protocol is deceptively complex. While the _Yjs/Yrs_ CRDT library is formally proven to converge, the **system around it** — message ordering, server relay, broadcast channels, offline reconnection, compaction, the index document, and critically the **diff layer** that converts file content to CRDT operations — introduces many places where bugs can hide:

| Risk Area             | Example Bug                                                                                     |
| --------------------- | ----------------------------------------------------------------------------------------------- |
| **Message relay**     | Server echoes updates back to the sender → infinite feedback loop (Issue #2)                    |
| **Channel lifecycle** | New doc updates are dropped if no broadcast channel exists yet (fuzzer-found divergence bug)    |
| **Diff layer**        | Stale file content fed into `dissimilar::diff` generates DELETE ops for valid data (Issue #6)   |
| **Client divergence** | Obsidian's `ignoreChanges` timeout vs folder client's sync-before-apply lead to different races |
| **Compaction**        | Client reconnects after history was compacted → applies update against missing base state       |
| **Index document**    | Concurrent insertions into `__index__` from multiple connections corrupt the doc list           |

Testing catches many of these, but testing only explores specific execution traces. **TLA+ exhaustively explores _all_ possible interleavings**, including rare race conditions that are impractical to trigger in tests.

### What TLA+ Will and Won't Verify

**In scope** (what TLA+ can verify):

- Convergence: all clients eventually reach the same document state
- No message duplication: updates are not echoed back to the sender
- No lost updates: every update is eventually delivered to all connected clients
- Server persistence: all accepted updates are persisted before broadcast
- Compaction safety: compacted documents remain equivalent to the full history
- Index consistency: the `__index__` document accurately reflects all known documents
- **Diff-layer safety**: stale file reads fed into the differ never cause data loss
- **Cross-client equivalence**: both Obsidian and folder clients reach the same state under identical scenarios

**Out of scope** (better verified by other means):

- Yrs CRDT merge correctness (proven by the Yjs/YATA literature)
- The correctness of `dissimilar::diff` itself (it is a well-tested Myers diff library)
- Binary encoding correctness (`protocol.rs` — unit tests are sufficient)
- WebSocket transport reliability (handled by the transport layer)
- SQLite transactional guarantees (handled by SQLite itself)

---

## 2. TLA+ Primer

For team members unfamiliar with TLA+, here is a minimal introduction.

### Core Concepts

| Concept               | Description                                                               |
| --------------------- | ------------------------------------------------------------------------- |
| **State**             | A snapshot of all variable values at a point in time                      |
| **Behavior**          | An infinite sequence of states (a "trace")                                |
| **Action**            | A relation between a current state and a next state (a state transition)  |
| **Specification**     | A formula that constrains the set of valid behaviors                      |
| **Invariant**         | A predicate that must hold in _every_ reachable state                     |
| **Temporal Property** | A predicate about the _sequence_ of states (e.g., "eventually X happens") |

### Tooling

| Tool                                                                                           | Purpose                                                                   |
| ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| [TLA+ Toolbox](https://lamport.azurewebsites.net/tla/toolbox.html)                             | IDE with integrated model checker                                         |
| [TLC](https://lamport.azurewebsites.net/tla/tools.html)                                        | Explicit-state model checker — exhaustively explores all reachable states |
| [Apalache](https://apalache.informal.systems/)                                                 | Symbolic model checker — uses SMT solvers, better for unbounded models    |
| [VS Code extension](https://marketplace.visualstudio.com/items?itemName=alygin.vscode-tlaplus) | Syntax highlighting, parse/check integration                              |

### Getting Started

```bash
# Install the TLA+ Toolbox (Java-based)
brew install --cask tla-plus-toolbox

# Or use the VS Code extension (recommended for daily use)
code --install-extension alygin.vscode-tlaplus
```

---

## 3. System Model

The TLA+ specification models Syncline as a distributed system with the following components.
Critically, the model must capture the **three-tier architecture** of each client: the CRDT document, the file on disk, and the diff layer that bridges them.

```
┌──────────────────────┐                           ┌──────────────────────┐
│ Client A (Obsidian)  │                           │ Client B (Folder)    │
│                      │                           │                      │
│  ┌────────────────┐  │     WebSocket              │  ┌────────────────┐  │
│  │  Obsidian API  │  │  ◄──────────────►         │  │  fs::read/write│  │
│  │  vault.modify()│  │                           │  │  notify watcher│  │
│  └───────┬────────┘  │       ┌─────────┐         │  └───────┬────────┘  │
│          │ debounce   │       │  Server │         │          │ debounce  │
│     ┌────▼─────┐     │       │         │         │     ┌────▼─────┐    │
│     │   diff   │     │ ◄───► │   DB    │ ◄─────► │     │   diff   │    │
│     │dissimilar│     │       │ Channels│         │     │dissimilar│    │
│     └────┬─────┘     │       └─────────┘         │     └────┬─────┘    │
│     ┌────▼─────┐     │                           │     ┌────▼─────┐    │
│     │ Yrs CRDT │     │                           │     │ Yrs CRDT │    │
│     │ (WASM)   │     │                           │     │ (.bin)   │    │
│     └──────────┘     │                           │     └──────────┘    │
└──────────────────────┘                           └──────────────────────┘
```

### 3.1 Constants and Parameters

```tla
CONSTANTS
    Clients,          \* Set of client identifiers, e.g., {"A", "B", "C"}
    DocIds,           \* Set of document identifiers, e.g., {"doc1", "doc2"}
    MaxUpdates,       \* Bound on total updates (for finite model checking)
    CompactionThreshold  \* Number of updates before server compacts
```

To keep the state space tractable for TLC, we recommend starting with:

- `Clients = {"A", "B"}` (2 clients)
- `DocIds = {"d1"}` (1 document)
- `MaxUpdates = 4` (4 total updates)
- `CompactionThreshold = 3`

This is sufficient to explore all interesting interleavings. The number of clients and documents can be increased later once the base spec is validated.

### 3.2 Variables

The specification tracks the following state. Notably, each client has **two** representations of each document: the CRDT document and the file on disk. The gap between these two is the source of the most subtle bugs.

```tla
VARIABLES
    \* --- Client CRDT State ---
    clientDoc,        \* [Clients -> [DocIds -> Seq(Update)]]
                      \*   Each client's local CRDT document, modeled as
                      \*   the ordered sequence of updates it has applied.

    \* --- Client Disk State (the diff layer) ---
    clientDisk,       \* [Clients -> [DocIds -> Seq(Update)]]
                      \*   The content currently written to disk. This may
                      \*   DIVERGE from clientDoc during the window between
                      \*   a remote update being applied to the CRDT and
                      \*   the file being written/read back.

    clientIgnoring,   \* [Clients -> SUBSET DocIds]
                      \*   The set of docs for which the file-watcher is
                      \*   suppressed (Obsidian's `ignoreChanges` set, or
                      \*   the folder client's disk==crdt guard).

    \* --- Client Connection State ---
    clientConnected,  \* [Clients -> BOOLEAN]
                      \*   Whether each client has an active WebSocket.

    clientSubscribed, \* [Clients -> SUBSET DocIds]
                      \*   The set of doc_ids this client has sent
                      \*   MSG_SYNC_STEP_1 for (i.e., is subscribed to
                      \*   the broadcast channel on the server).

    clientType,       \* [Clients -> {"obsidian", "folder"}]
                      \*   Which client variant. This determines the
                      \*   feedback suppression strategy.

    \* --- Network ---
    clientToServer,   \* [Clients -> Seq(Message)]
                      \*   In-flight messages from each client to the server.
                      \*   Modeled as an ordered queue (FIFO per-connection).

    serverToClient,   \* [Clients -> Seq(Message)]
                      \*   In-flight messages from the server to each client.
                      \*   Modeled as an ordered queue.

    \* --- Server State ---
    serverDB,         \* [DocIds -> Seq(Update)]
                      \*   Server's persistent storage: ordered log of updates
                      \*   per document.

    serverChannels,   \* [DocIds -> SUBSET Clients]
                      \*   Which clients are subscribed to each document's
                      \*   broadcast channel on the server.

    serverIndex,      \* SUBSET DocIds
                      \*   The set of doc_ids registered in __index__.

    \* --- Global ---
    updateCounter     \* Nat
                      \*   Monotonically increasing counter, used to assign
                      \*   unique IDs to each update for provenance tracking.
```

### 3.3 Abstracting CRDTs

The key modeling decision is **how to abstract the CRDT**. We do _not_ model Yrs character-level operations. Instead, we model each update as an **opaque token** with a unique ID. Two documents are "converged" when they have applied the exact same _set_ of updates (regardless of order), because the CRDT guarantees order-independence.

This abstraction is valid because:

1. Yrs is independently proven to be a correct CRDT (commutativity, idempotency, associativity).
2. We are verifying the _protocol layer_ — whether the right updates reach the right places — not the merge algorithm itself.

```tla
Update == [id: Nat, origin: Clients, docId: DocIds]
\* An update is uniquely identified by its `id`, originated from `origin`,
\* and targets document `docId`.
```

---

## 4. Actions (State Transitions)

The specification defines the following actions, each corresponding to an event in the real system:

### 4.1 Client Actions

#### `ClientEdit(c, d)`

A client `c` makes a local edit to document `d`.

```tla
ClientEdit(c, d) ==
    /\ clientConnected[c]                 \* Must be connected (or offline — see below)
    /\ updateCounter < MaxUpdates         \* Bound for model checking
    /\ LET u == [id |-> updateCounter + 1, origin |-> c, docId |-> d]
       IN /\ clientDoc' = [clientDoc EXCEPT ![c][d] = Append(@, u)]
          /\ updateCounter' = updateCounter + 1
          \* If connected, enqueue MSG_UPDATE to server
          /\ IF clientConnected[c]
             THEN clientToServer' = [clientToServer EXCEPT
                    ![c] = Append(@, [type |-> "MSG_UPDATE", docId |-> d, update |-> u])]
             ELSE UNCHANGED clientToServer
    /\ UNCHANGED <<clientConnected, clientSubscribed, serverToClient,
                   serverDB, serverChannels, serverIndex>>
```

#### `ClientGoOffline(c)`

A client disconnects. Any in-flight messages remain in the queue but the server must detect the close.

```tla
ClientGoOffline(c) ==
    /\ clientConnected[c]
    /\ clientConnected' = [clientConnected EXCEPT ![c] = FALSE]
    /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = {}]
    \* Server removes client from all channels
    /\ serverChannels' = [d \in DocIds |-> serverChannels[d] \ {c}]
    /\ UNCHANGED <<clientDoc, clientToServer, serverToClient,
                   serverDB, serverIndex, updateCounter>>
```

#### `ClientReconnect(c)`

A client comes back online. It sends `MSG_SYNC_STEP_1` for each document it knows about.

```tla
ClientReconnect(c) ==
    /\ ~clientConnected[c]
    /\ clientConnected' = [clientConnected EXCEPT ![c] = TRUE]
    \* Enqueue SYNC_STEP_1 for every document the client has locally
    /\ LET knownDocs == {d \in DocIds : clientDoc[c][d] /= <<>>}
           syncMsgs == {[type |-> "MSG_SYNC_STEP_1",
                         docId |-> d,
                         stateVector |-> UpdateIds(clientDoc[c][d])]
                        : d \in knownDocs}
       IN clientToServer' = [clientToServer EXCEPT
            ![c] = @ \o SetToSeq(syncMsgs)]
    /\ UNCHANGED <<clientDoc, clientSubscribed, serverToClient,
                   serverDB, serverChannels, serverIndex, updateCounter>>
```

#### `ClientReceive(c)`

A client processes the next message in its incoming queue.

```tla
ClientReceive(c) ==
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN CASE msg.type = "MSG_SYNC_STEP_2" ->
                \* Apply the diff — only updates the client doesn't already have
                LET newUpdates == {u \in msg.updates : u.id \notin UpdateIds(clientDoc[c][msg.docId])}
                IN clientDoc' = [clientDoc EXCEPT
                     ![c][msg.docId] = @ \o SetToSeq(newUpdates)]
            [] msg.type = "MSG_UPDATE" ->
                \* Apply the single update if not already applied
                IF msg.update.id \notin UpdateIds(clientDoc[c][msg.docId])
                THEN clientDoc' = [clientDoc EXCEPT
                       ![c][msg.docId] = Append(@, msg.update)]
                ELSE UNCHANGED clientDoc
          /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientConnected, clientSubscribed, clientToServer,
                   serverDB, serverChannels, serverIndex, updateCounter>>
```

### 4.2 Server Actions

#### `ServerReceiveSyncStep1(c)`

Server processes a `MSG_SYNC_STEP_1` from client `c`.

```tla
ServerReceiveSyncStep1(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = "MSG_SYNC_STEP_1"
    /\ LET msg == Head(clientToServer[c])
           d   == msg.docId
           sv  == msg.stateVector
           \* Compute the diff: all server-side updates the client doesn't have
           diff == {u \in ToSet(serverDB[d]) : u.id \notin sv}
       IN
       \* Subscribe the client to the broadcast channel
       /\ serverChannels' = [serverChannels EXCEPT ![d] = @ \cup {c}]
       /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = @ \cup {d}]
       \* Register doc in __index__ if new
       /\ serverIndex' = serverIndex \cup {d}
       \* Send MSG_SYNC_STEP_2 with the diff
       /\ serverToClient' = [serverToClient EXCEPT
            ![c] = Append(@, [type |-> "MSG_SYNC_STEP_2", docId |-> d, updates |-> diff])]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientConnected, serverDB, updateCounter>>
```

#### `ServerReceiveUpdate(c)`

Server processes a `MSG_UPDATE` from client `c`.

```tla
ServerReceiveUpdate(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = "MSG_UPDATE"
    /\ LET msg == Head(clientToServer[c])
           d   == msg.docId
           u   == msg.update
       IN
       \* 1. Persist BEFORE broadcasting (matches real implementation)
       /\ serverDB' = [serverDB EXCEPT ![d] = Append(@, u)]
       \* 2. Register in __index__ if new
       /\ serverIndex' = serverIndex \cup {d}
       \* 3. Broadcast to all subscribers EXCEPT the sender
       /\ LET recipients == serverChannels[d] \ {c}
          IN serverToClient' = [s \in Clients |->
               IF s \in recipients
               THEN Append(serverToClient[s],
                           [type |-> "MSG_UPDATE", docId |-> d, update |-> u])
               ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientConnected, clientSubscribed,
                   serverChannels, updateCounter>>
```

#### `ServerCompact(d)`

Server compacts the update log for document `d`.

```tla
ServerCompact(d) ==
    /\ Len(serverDB[d]) >= CompactionThreshold
    \* The compacted representation is a single "snapshot" update
    \* that contains the merged set of all individual update IDs.
    /\ LET allIds == {u.id : u \in ToSet(serverDB[d])}
           snapshot == [id |-> -1, origin |-> "server", docId |-> d,
                        mergedIds |-> allIds]
       IN serverDB' = [serverDB EXCEPT ![d] = <<snapshot>>]
    /\ UNCHANGED <<clientDoc, clientConnected, clientSubscribed,
                   clientToServer, serverToClient, serverChannels,
                   serverIndex, updateCounter>>
```

### 4.3 Next-State Relation

```tla
Next ==
    \/ \E c \in Clients, d \in DocIds : ClientEdit(c, d)
    \/ \E c \in Clients : ClientGoOffline(c)
    \/ \E c \in Clients : ClientReconnect(c)
    \/ \E c \in Clients : ClientReceive(c)
    \/ \E c \in Clients : ServerReceiveSyncStep1(c)
    \/ \E c \in Clients : ServerReceiveUpdate(c)
    \/ \E d \in DocIds  : ServerCompact(d)
```

---

## 5. Properties to Verify

### 5.1 Safety Properties (Invariants)

These must hold in **every** reachable state.

#### `TypeOK` — Type Invariant

Ensures all variables stay within their declared domains. This catches modeling errors early.

```tla
TypeOK ==
    /\ clientDoc \in [Clients -> [DocIds -> Seq(Update)]]
    /\ clientConnected \in [Clients -> BOOLEAN]
    /\ clientSubscribed \in [Clients -> SUBSET DocIds]
    /\ serverDB \in [DocIds -> Seq(Update)]
    /\ serverChannels \in [DocIds -> SUBSET Clients]
    /\ serverIndex \in SUBSET DocIds
    /\ updateCounter \in Nat
```

#### `NoEcho` — Updates Never Echoed Back to Sender

The server must never send an update back to the client that originated it. This was a real bug (Issue #2).

```tla
NoEcho ==
    \A c \in Clients :
        \A i \in 1..Len(serverToClient[c]) :
            LET msg == serverToClient[c][i]
            IN msg.type = "MSG_UPDATE" => msg.update.origin /= c
```

#### `PersistBeforeBroadcast` — Updates Are Persisted Before Broadcasting

The server must persist an update to SQLite _before_ broadcasting it. This ensures no concurrent `SyncStep2` response is missing recent updates.

```tla
PersistBeforeBroadcast ==
    \* If a MSG_UPDATE for update u is in any client's serverToClient queue,
    \* then u must already be in serverDB.
    \A c \in Clients :
        \A i \in 1..Len(serverToClient[c]) :
            LET msg == serverToClient[c][i]
            IN (msg.type = "MSG_UPDATE") =>
               msg.update \in ToSet(serverDB[msg.docId])
```

#### `IndexConsistency` — Index Reflects All Known Documents

Every document that exists in `serverDB` with at least one update must be registered in `serverIndex`.

```tla
IndexConsistency ==
    \A d \in DocIds :
        serverDB[d] /= <<>> => d \in serverIndex
```

#### `ChannelOnlyForConnected` — Only Connected Clients In Channels

Only connected clients should be subscribed to broadcast channels.

```tla
ChannelOnlyForConnected ==
    \A d \in DocIds :
        \A c \in serverChannels[d] :
            clientConnected[c]
```

### 5.2 Liveness Properties (Temporal)

These assert that certain conditions _eventually_ become true, given fairness assumptions.

> **Fairness**: We assume _weak fairness_ on all server processing actions (the server eventually processes any message in its incoming queue) and on `ClientReceive` (clients eventually process incoming messages). We do _not_ assume fairness on `ClientEdit`, `ClientGoOffline`, or `ClientReconnect` — the environment is adversarial.

#### `EventualConvergence` — Eventual Consistency

If all clients are connected and no new edits occur, all clients eventually converge to the same document state.

```tla
EventualConvergence ==
    \A d \in DocIds :
        \* If all clients are connected and all queues are empty...
        <>( (\A c \in Clients : clientConnected[c])
            /\ (\A c \in Clients : clientToServer[c] = <<>>)
            /\ (\A c \in Clients : serverToClient[c] = <<>>)
            \* ...then all clients have the same set of update IDs
            => (\A c1, c2 \in Clients :
                  UpdateIds(clientDoc[c1][d]) = UpdateIds(clientDoc[c2][d])))
```

#### `EventualDelivery` — No Lost Updates

Every update that enters the server is eventually delivered to all connected and subscribed clients.

```tla
EventualDelivery ==
    \A u \in Update, d \in DocIds :
        \* If u is persisted in the server DB...
        [](u \in ToSet(serverDB[d]) =>
            \* ...then for every client subscribed to d, eventually u is applied
            \A c \in Clients :
                (c \in serverChannels[d] /\ clientConnected[c]) =>
                    <>(u.id \in UpdateIds(clientDoc[c][d])))
```

#### `EventualIndexPropagation` — All Clients Learn About All Documents

Every document registered in `serverIndex` is eventually known to all connected clients.

```tla
EventualIndexPropagation ==
    \A d \in DocIds :
        [](d \in serverIndex =>
            \A c \in Clients :
                clientConnected[c] => <>(d \in clientSubscribed[c]))
```

---

## 6. The Diff Layer

The diff layer is the most critical part of the model that the base CRDT abstraction does _not_ cover. Both clients use `dissimilar::diff(old_content, new_content)` to compute a minimal edit script, then translate each diff chunk into Yrs `insert`/`remove_range` operations. This is the code path in:

- **Folder client**: `apply_diff_to_yrs()` in `syncline/src/client/diff.rs`
- **Obsidian plugin**: `SynclineClient::update()` in `syncline/src/wasm_client.rs`

The diff layer is **not** just a projection — it is a lossy transformation with timing-dependent behavior:

### 6.1 Why the Diff Layer Matters for Formal Verification

```
        File on disk          dissimilar::diff()          Yrs CRDT
        ────────────    ──────────────────────────►    ──────────────
        "Hel"            diff("Hello", "Hel")           DELETE @ 3..5
        (stale!)         = [Equal("Hel"), Delete("lo")]  (spurious!)
```

The diff compares `old_content` (the current CRDT text) against `new_content` (what was read from disk). If the disk content is **stale** — i.e., written at time T₁ but read back at T₂ after the CRDT has received newer updates — the diff will generate DELETE operations for the characters that were added between T₁ and T₂. This is exactly Issue #6.

The key insight: **the diff is always correct for the inputs it receives**, but the inputs can be wrong due to timing. TLA+ can exhaustively check all possible timings.

### 6.2 Modeling the Diff as a State Transition

Instead of modeling the diff algorithm itself, we model the **observable effect**: the diff reads `clientDisk[c][d]` and the current `clientDoc[c][d]`, and produces an update that makes the CRDT match the disk.

```tla
\* The diff operation: client reads disk, diffs against CRDT, generates update.
\* This is the CORE action where stale-read bugs manifest.
ClientDiffFromDisk(c, d) ==
    /\ clientConnected[c]
    /\ d \notin clientIgnoring[c]       \* File-watcher is not suppressed
    /\ clientDisk[c][d] /= clientDoc[c][d]  \* Disk differs from CRDT
    /\ LET diskIds    == UpdateIds(clientDisk[c][d])
           crdtIds    == UpdateIds(clientDoc[c][d])
           \* Updates in CRDT but not on disk → would be DELETED by diff
           toDelete   == crdtIds \ diskIds
           \* Updates on disk but not in CRDT → would be INSERTED by diff
           toInsert   == diskIds \ crdtIds
       IN
       \* The diff produces a compound update that:
       \*   1. Removes content the CRDT has but disk doesn't (DANGEROUS if stale)
       \*   2. Inserts content the disk has but CRDT doesn't
       /\ LET u == [id |-> updateCounter + 1, origin |-> c, docId |-> d,
                    deletes |-> toDelete, inserts |-> toInsert]
          IN /\ clientDoc' = [clientDoc EXCEPT ![c][d] =
                    \* Remove deleted IDs, add the new compound update
                    SelectSeq(@, LAMBDA x : x.id \notin toDelete) \o <<u>>]
             /\ updateCounter' = updateCounter + 1
             /\ clientToServer' = [clientToServer EXCEPT
                  ![c] = Append(@, [type |-> "MSG_UPDATE", docId |-> d, update |-> u])]
    /\ UNCHANGED <<clientDisk, clientIgnoring, clientConnected, clientSubscribed,
                   serverToClient, serverDB, serverChannels, serverIndex>>
```

### 6.3 Modeling Disk Writes (Remote Update → File)

When a remote update arrives, the client writes the merged CRDT content to disk. This is where the `clientDisk` variable diverges from `clientDoc`:

```tla
\* After applying a remote update, write the CRDT content to disk
\* and suppress the file-watcher for this document.
ClientWriteToDisk(c, d) ==
    /\ clientDoc[c][d] /= clientDisk[c][d]  \* CRDT was updated
    /\ clientDisk' = [clientDisk EXCEPT ![c][d] = clientDoc[c][d]]
    \* Suppress file-watcher (Obsidian: ignoreChanges.add; Folder: disk==crdt check)
    /\ clientIgnoring' = [clientIgnoring EXCEPT ![c] = @ \cup {d}]
    /\ UNCHANGED <<all other variables>>
```

### 6.4 Modeling the Ignore Timer Expiry

The `ignoreChanges` suppression eventually expires. In Obsidian, this is a 500ms `setTimeout`. In the folder client, the check is `disk_content != yjs_content`. The TLA+ model abstracts this as a non-deterministic action:

```tla
\* The file-watcher suppression expires.
\* In Obsidian: setTimeout(() => ignoreChanges.delete(path), 500)
\* In Folder: implicit via disk==crdt comparison before diffing
ClientIgnoreExpires(c, d) ==
    /\ d \in clientIgnoring[c]
    /\ clientIgnoring' = [clientIgnoring EXCEPT ![c] = @ \ {d}]
    /\ UNCHANGED <<all other variables>>
```

**The bug window**: Between `ClientWriteToDisk` and `ClientIgnoreExpires`, if a _new_ remote update arrives and changes `clientDoc` but the disk still has the old content, then after `ClientIgnoreExpires` fires, `ClientDiffFromDisk` will read the stale disk and generate spurious deletes.

---

## 7. Obsidian vs Folder Client Differences

The two client types have materially different architectures that affect synchronization correctness. The TLA+ model must capture these differences.

### 7.1 Architectural Comparison

| Aspect                       | Obsidian Plugin (`wasm_client.rs` + `main.ts`)                              | Folder Client (`client/app.rs`)                                             |
| ---------------------------- | --------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| **Change detection**         | `vault.on('modify')` → `debounce(300ms)` → `client.update(uuid, content)`   | `notify` crate → `DebouncedWatcher(300ms)` → `apply_diff_to_yrs()`          |
| **Diff function**            | `SynclineClient::update()` inline diff in WASM                              | `apply_diff_to_yrs()` in `diff.rs`                                          |
| **Remote update → disk**     | Callback → `onRemoteUpdate()` → `vault.read()` → compare → `vault.modify()` | Inline in event loop → `fs::read_to_string()` → compare → `fs::write()`     |
| **Feedback suppression**     | `ignoreChanges: Set<string>` with **500ms timer**                           | Compares `disk_content != yjs_content` **before** diffing                   |
| **Pre-apply local diff**     | ❌ **Not done** — applies remote first, reads local on next modify          | ✅ Reads disk, applies local diff to CRDT **before** applying remote update |
| **CRDT persistence**         | In-memory only (lost on Obsidian restart)                                   | Persisted to `.syncline/data/{uuid}.bin`                                    |
| **Rename detection**         | `vault.on('rename')` event → `set_meta_path()`                              | Content-hash matching across delete+create in watcher batch                 |
| **Path conflict resolution** | Server UUID wins silently                                                   | `resolve_path_conflict()` → renames local to `(client-name).md`             |
| **Offline bootstrap**        | Re-registers all vault files on connect, pushes full state as `MSG_UPDATE`  | `bootstrap_offline_changes()` — diffs all files against `.bin` state        |

### 7.2 Implications for the TLA+ Model

The most important difference is the **pre-apply local diff** in the folder client. When the folder client receives a remote update, it:

1. Reads the current disk content
2. Compares disk content to the current CRDT text
3. If they differ, applies the local diff to the CRDT **first** (broadcasting the local changes)
4. Then applies the remote update

The Obsidian client does **not** do this. It applies the remote update directly, then writes the merged result to disk, relying on `ignoreChanges` to suppress the file-watcher echo.

This means the Obsidian client has a vulnerability window that the folder client does not:

```
Obsidian client timeline:
  t=0ms   Remote update arrives → CRDT updated to "Hello"
  t=0ms   vault.modify("Hello") called → ignoreChanges.add(path)
  t=100ms User types locally → file becomes "Hello World"
  t=300ms debounced onFileModify fires → reads "Hello World"
          BUT ignoreChanges still active (500ms timeout)
          → change is SWALLOWED. "World" is lost.
  t=500ms ignoreChanges.delete(path)
  t=600ms Next modify event → "Hello World" is finally diffed and sent
          (only if the user types again; otherwise the edit is silently lost)

Folder client timeline:
  t=0ms   Remote update arrives
  t=0ms   Reads disk → "Hello World" (user typed offline)
  t=0ms   disk != crdt → applies local diff FIRST (broadcasts "World")
  t=0ms   Then applies remote update → CRDT = "Hello World" (merged)
  t=0ms   Writes "Hello World" to disk → disk == crdt, no echo
```

### 7.3 Modeling Client Types in TLA+

To model these differences, we introduce a `clientType` constant and make the `ClientReceive` and `ClientDiffFromDisk` actions conditional:

```tla
CONSTANTS
    ClientTypes  \* Function: Clients -> {"obsidian", "folder"}

\* Folder client: pre-apply local diff before remote update
FolderClientReceive(c) ==
    /\ ClientTypes[c] = "folder"
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN
       \* Step 1: If disk differs from CRDT, diff and broadcast FIRST
       /\ IF clientDisk[c][msg.docId] /= clientDoc[c][msg.docId]
          THEN \* Generate a local update from disk content
               LET localU == ... \* diff(crdt, disk)
               IN /\ clientDoc' = [clientDoc EXCEPT ![c][msg.docId] = <apply local + remote>]
                  /\ clientToServer' = [clientToServer EXCEPT
                       ![c] = Append(@, [type |-> "MSG_UPDATE", ...])]
          ELSE \* Just apply remote update
               /\ clientDoc' = [clientDoc EXCEPT ![c][msg.docId] = <apply remote>]
       \* Step 2: Write merged content to disk (no ignore needed — disk == crdt)
       /\ clientDisk' = [clientDisk EXCEPT ![c][msg.docId] = clientDoc'[c][msg.docId]]
       /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]

\* Obsidian client: apply remote, write to disk, set ignore timer
ObsidianClientReceive(c) ==
    /\ ClientTypes[c] = "obsidian"
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN
       \* Apply remote update to CRDT (WITHOUT pre-diffing disk)
       /\ clientDoc' = [clientDoc EXCEPT ![c][msg.docId] = <apply remote>]
       \* Write to disk and suppress watcher
       /\ clientDisk' = [clientDisk EXCEPT ![c][msg.docId] = clientDoc'[c][msg.docId]]
       /\ clientIgnoring' = [clientIgnoring EXCEPT ![c] = @ \cup {msg.docId}]
       /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]
```

### 7.4 Properties Specific to Client Differences

#### `DiskCrdtEventualConsistency` — Disk Eventually Matches CRDT

For every client, the disk content must eventually match the CRDT content (no permanently lost writes).

```tla
DiskCrdtEventualConsistency ==
    \A c \in Clients, d \in DocIds :
        [](clientConnected[c] /\ d \in clientSubscribed[c] =>
            <>(clientDisk[c][d] = clientDoc[c][d]))
```

#### `NoSwallowedEdits` — Local Edits Are Never Silently Dropped

If a user makes an edit (disk changes), it must eventually be reflected in the CRDT, even if `ignoreChanges` temporarily suppresses the file-watcher.

```tla
NoSwallowedEdits ==
    \A c \in Clients, d \in DocIds :
        \* If disk has content that CRDT doesn't...
        [](\E u \in ToSet(clientDisk[c][d]) : u.id \notin UpdateIds(clientDoc[c][d])
           \* ...then eventually it appears in the CRDT
           => <>(u.id \in UpdateIds(clientDoc[c][d])))
```

#### `NoSpuriousDeletes` — Stale Reads Never Cause Data Loss

The most valuable property to verify. A diff that reads stale disk content must never cause a non-local update to be deleted.

```tla
NoSpuriousDeletes ==
    \* After ClientDiffFromDisk(c, d), no update originally from another
    \* client should have been removed from c's CRDT.
    \A c \in Clients, d \in DocIds :
        \A u \in Update :
            (u.origin /= c /\ u.id \in UpdateIds(clientDoc[c][d]))
            => [](u.id \in UpdateIds(clientDoc[c][d]))
```

> **Note**: In the real system, `NoSpuriousDeletes` is currently _violated_ by the Obsidian client under the Issue #6 race condition. The TLA+ model should first confirm this violation exists, then verify that proposed fixes (such as epoch-based suppression or the folder client's pre-apply strategy) eliminate it.

---

## 8. Additional Modeling Extensions

Once the base specification (including the diff layer and client types) is validated, consider adding these refinements.

### 8.1 Message Reordering

The base model assumes FIFO channels (matching TCP/WebSocket semantics). To test resilience against out-of-order delivery (e.g., after a disconnection), replace the `Seq(Message)` channels with _sets_ of messages (bag semantics):

```tla
\* Non-FIFO variant: server picks any pending message, not just the head
ServerReceiveAny(c) ==
    /\ clientToServer[c] /= {}
    /\ \E msg \in clientToServer[c] :
        \* ... process msg ...
        /\ clientToServer' = [clientToServer EXCEPT ![c] = @ \ {msg}]
```

### 8.2 Message Loss

To model unreliable networks (though WebSocket over TCP is reliable, this is useful for testing protocol robustness):

```tla
MessageDrop(c) ==
    /\ clientToServer[c] /= <<>>
    /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<all other variables>>
```

### 8.3 Offline Edits and Reconnection Bootstrap

The real Syncline client performs a `bootstrap_offline_changes` step on reconnection — it diffs the local filesystem against the CRDT state and generates updates. Model this by allowing `ClientEdit` when `~clientConnected[c]` and then sending all accumulated updates on `ClientReconnect`.

### 8.4 Rename Propagation

The two clients detect renames differently. The Obsidian client gets a rename event directly from the Obsidian API, while the folder client uses content-hash matching across a batch of delete+create events. Model this as:

```tla
\* Obsidian: atomic rename — old path removed, new path inserted, same UUID
ObsidianRename(c, d, newPath) ==
    /\ ClientTypes[c] = "obsidian"
    /\ \* Update meta.path in the CRDT, broadcast via observe_update_v1
    ...

\* Folder: content-matching heuristic — may fail if content was also modified
FolderRename(c, d, newPath) ==
    /\ ClientTypes[c] = "folder"
    /\ \* Match deleted file's CRDT content against new file's disk content
    /\ \* If they match → treat as rename; if not → treat as delete + create
    ...
```

The property to verify: `RenameConvergence` — after a rename on one client, all clients eventually agree on the new path.

---

## 7. Running the Model Checker

### 7.1 TLC Configuration File (`SynclineSync.cfg`)

```
SPECIFICATION Spec

CONSTANTS
    Clients = {"A", "B"}
    DocIds = {"d1"}
    MaxUpdates = 4
    CompactionThreshold = 3

INVARIANTS
    TypeOK
    NoEcho
    PersistBeforeBroadcast
    IndexConsistency
    ChannelOnlyForConnected

PROPERTIES
    EventualConvergence
    EventualDelivery
```

### 7.2 Running TLC

```bash
# From the TLA+ Toolbox: Model → New Model → Run
# Or from command line:
java -jar tla2tools.jar -config SynclineSync.cfg SynclineSync.tla

# For larger models, increase heap:
java -Xmx8g -jar tla2tools.jar -workers 8 -config SynclineSync.cfg SynclineSync.tla
```

### 7.3 Expected State Space

For `Clients = {"A", "B"}, DocIds = {"d1"}, MaxUpdates = 4`:

| Metric                  | Estimated Value |
| ----------------------- | --------------- |
| Distinct states         | ~10,000–50,000  |
| State-space diameter    | ~20–30 steps    |
| Time on modern hardware | < 1 minute      |

For `Clients = {"A", "B", "C"}, DocIds = {"d1", "d2"}, MaxUpdates = 5`:

| Metric                  | Estimated Value |
| ----------------------- | --------------- |
| Distinct states         | ~1,000,000+     |
| Time on modern hardware | 5–30 minutes    |

### 7.4 Interpreting Results

- **"No violation found"**: The property held across all reachable states. This is a strong guarantee within the bounded model.
- **"Invariant X violated"**: TLC produces a minimal counterexample trace. This trace shows the exact sequence of actions that leads to the violation — extremely valuable for debugging.
- **"Temporal property Y violated"**: TLC produces a lasso-shaped counterexample (a prefix + a cycle). This indicates a scenario where the system can loop forever without satisfying the liveness condition.

---

## 9. File Structure

We recommend the following file structure under `docs/tla/`:

```
docs/tla/
├── SynclineSync.tla              # Main specification module
├── SynclineSync.cfg              # TLC model checking configuration
├── SynclineSyncDiffLayer.tla     # Extension: diff + disk model
├── SynclineSyncClientTypes.tla   # Extension: Obsidian vs folder client
├── SynclineSyncCompaction.tla    # Extension: compaction model
├── SynclineSyncStaleRead.tla     # Extension: stale file-watcher race
├── SynclineSyncLossy.tla         # Extension: message loss/reordering
└── README.md                     # Verification results and notes
```

---

## 10. Mapping TLA+ Actions to Syncline Source Code

Each TLA+ action corresponds to a specific code path in the Rust/TypeScript implementation. This traceability matrix ensures the model accurately reflects reality.

### Server Actions

| TLA+ Action              | Source File                     | Function / Code Path                       |
| ------------------------ | ------------------------------- | ------------------------------------------ |
| `ServerReceiveSyncStep1` | `syncline/src/server/server.rs` | `handle_socket` → `MSG_SYNC_STEP_1` branch |
| `ServerReceiveUpdate`    | `syncline/src/server/server.rs` | `handle_socket` → `MSG_UPDATE` branch      |
| `ServerCompact`          | Not yet implemented             | See Design Doc §6 (Lazy Compaction)        |

### Folder Client Actions

| TLA+ Action           | Source File                      | Function / Code Path                                                      |
| --------------------- | -------------------------------- | ------------------------------------------------------------------------- |
| `ClientEdit`          | `syncline/src/client/app.rs`     | `watcher_rx.recv()` → `apply_diff_to_yrs()` L579                          |
| `ClientDiffFromDisk`  | `syncline/src/client/diff.rs`    | `apply_diff_to_yrs(doc, text_ref, old, new)` L4                           |
| `FolderClientReceive` | `syncline/src/client/app.rs`     | `app_rx.recv()` → pre-apply local diff L258–271 → `apply_update` L274–277 |
| `ClientWriteToDisk`   | `syncline/src/client/app.rs`     | `fs::write(&phys_path, &text_val)` L380                                   |
| `ClientGoOffline`     | `syncline/src/client/network.rs` | WebSocket close / `Drop for SynclineClient`                               |
| `ClientReconnect`     | `syncline/src/client/app.rs`     | `bootstrap_offline_changes()` → `MSG_SYNC_STEP_1` L98–124                 |
| `FolderRename`        | `syncline/src/client/app.rs`     | Batch rename detection L444–508                                           |

### Obsidian Client Actions

| TLA+ Action             | Source File                   | Function / Code Path                                        |
| ----------------------- | ----------------------------- | ----------------------------------------------------------- |
| `ClientEdit`            | `obsidian-plugin/main.ts`     | `onFileModify` → `client.update(uuid, content)` L601        |
| `ClientDiffFromDisk`    | `syncline/src/wasm_client.rs` | `SynclineClient::update()` inline diff L348–387             |
| `ObsidianClientReceive` | `syncline/src/wasm_client.rs` | `onmessage` → `apply_update()` L131–138                     |
| `ClientWriteToDisk`     | `obsidian-plugin/main.ts`     | `onRemoteUpdate()` → `vault.modify()` L563                  |
| `ClientIgnoreExpires`   | `obsidian-plugin/main.ts`     | `setTimeout(() => ignoreChanges.delete(), 500)` L567        |
| `ClientGoOffline`       | `obsidian-plugin/main.ts`     | WebSocket `onclose` / `disconnect()` L436–447               |
| `ClientReconnect`       | `obsidian-plugin/main.ts`     | `connect()` → registers all files → `client.connect()` L244 |
| `ObsidianRename`        | `obsidian-plugin/main.ts`     | `onFileRename` → `set_meta_path()` L634–658                 |

---

## 11. Recommended Verification Roadmap

### Phase 1: Core Protocol (1–2 days)

1. Write the base `SynclineSync.tla` with 2 clients (same type), 1 document, no diff layer.
2. Verify `TypeOK`, `NoEcho`, `PersistBeforeBroadcast`.
3. Verify `EventualConvergence` with weak fairness.

### Phase 2: Diff Layer + Disk Model (2–3 days) ⭐ **Highest value**

1. Add `clientDisk`, `clientIgnoring`, and the diff/write/expire actions.
2. Reproduce Issue #6: verify that `NoSpuriousDeletes` is violated when the ignoreChanges timer expires after a remote update arrives.
3. Verify proposed mitigations: epoch-based suppression, or the folder client's pre-apply strategy.
4. Verify `DiskCrdtEventualConsistency` and `NoSwallowedEdits`.

### Phase 3: Client Type Differences (1–2 days)

1. Introduce `clientType` constant. Model `ObsidianClientReceive` vs `FolderClientReceive`.
2. Run with `ClientTypes = {"A" -> "obsidian", "B" -> "folder"}` to explore cross-client scenarios.
3. Verify that convergence holds even when one client is Obsidian and the other is folder.
4. Compare: does the folder client's pre-apply strategy provably prevent Issue #6?

### Phase 4: Multi-Document + Index (1 day)

1. Add `__index__` document and `serverIndex` state.
2. Verify `IndexConsistency` and `EventualIndexPropagation`.
3. Increase to 2 documents.

### Phase 5: Compaction (1 day)

1. Add `ServerCompact` action.
2. Verify that compaction preserves convergence.
3. Model the `ERR_HISTORY_LOST` recovery: a client that missed compacted history must accept the server snapshot.

### Phase 6: Rename Propagation (1 day)

1. Add `meta.path` updates and the two rename strategies.
2. Verify `RenameConvergence`: after a rename, all clients eventually agree on the path.
3. Test cross-client rename: Obsidian renames while folder client is offline, then reconnects.

### Phase 7: Stress Testing (optional)

1. Increase to 3 clients (2 folder + 1 Obsidian), 3 documents, 6 updates.
2. Add message loss and reordering.
3. Run overnight with `-workers 8`.

---

## 12. References

- **Lamport, L.** _Specifying Systems_ (2002) — The definitive TLA+ textbook.
- **Hillel Wayne.** [_Learn TLA+_](https://learntla.com/) — A practical, accessible introduction.
- **Markus Kuppe.** [_TLA+ Video Course_](https://lamport.azurewebsites.net/video/videos.html) — Leslie Lamport's lecture series.
- **Shapiro et al.** _Conflict-Free Replicated Data Types_ (2011) — CRDT foundations.
- **Attiya et al.** _Specification and Complexity of CRDTs_ (2016) — Formal CRDT properties.
- **Nicolaescu et al.** _Near Real-Time Peer-to-Peer Shared Editing on Extensible Data Types (YATA)_ (2016) — The Yjs algorithm paper.
- **Amazon / MongoDB / CockroachDB** have all published on using TLA+ for protocol verification in production systems.
