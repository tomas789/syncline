# 🔬 Formal Verification

Syncline's synchronization protocol is formally verified using [TLA+](https://lamport.azurewebsites.net/tla/tla.html) — a formal specification language designed by Leslie Lamport for modeling concurrent and distributed systems.

## Why Formal Verification?

While the Yjs/Yrs CRDT library is independently proven to converge, the **system around it** — message ordering, server relay, broadcast channels, offline reconnection, the diff layer, and the index document — introduces many places where subtle bugs can hide.

Testing catches many of these, but testing only explores specific execution traces. TLA+ exhaustively explores **all possible interleavings**, including rare race conditions that are impractical to trigger in tests. In fact, several real bugs were first predicted by the TLA+ model before being confirmed in the implementation:

| Risk Area             | Example Bug                                                                                     |
| --------------------- | ----------------------------------------------------------------------------------------------- |
| **Message relay**     | Server echoes updates back to the sender → infinite feedback loop (Issue #2)                    |
| **Channel lifecycle** | New doc updates are dropped if no broadcast channel exists yet (fuzzer-found divergence bug)    |
| **Diff layer**        | Stale file content fed into `dissimilar::diff` generates DELETE ops for valid data (Issue #6)   |
| **Client divergence** | Obsidian's `ignoreChanges` timeout vs folder client's sync-before-apply lead to different races |
| **Index document**    | Concurrent insertions into `__index__` from multiple connections corrupt the doc list           |

## What TLA+ Verifies

**In scope** (what TLA+ can verify):

- **Convergence**: All clients eventually reach the same document state
- **No echo**: Updates are not echoed back to the sender
- **No lost updates**: Every update is eventually delivered to all connected clients
- **Server persistence**: All accepted updates are persisted before broadcast
- **Index consistency**: The `__index__` document accurately reflects all known documents
- **Diff-layer safety**: Stale file reads fed into the differ never cause data loss
- **Cross-client equivalence**: Both Obsidian and folder clients reach the same state under identical scenarios
- **Rename convergence**: After a rename on one client, all clients eventually agree on the new path

**Out of scope** (better verified by other means):

- Yrs CRDT merge correctness (proven by the Yjs/YATA literature)
- Binary encoding correctness (unit tests are sufficient)
- WebSocket transport reliability (handled by the transport layer)
- SQLite transactional guarantees (handled by SQLite itself)

## System Model

The TLA+ specification models Syncline as a distributed system with the following components. Critically, the model captures the **three-tier architecture** of each client: the CRDT document, the file on disk, and the diff layer that bridges them.

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

CRDTs are abstracted as opaque update tokens — two documents are "converged" when they have applied the exact same _set_ of updates (regardless of order), because the CRDT guarantees order-independence. This keeps the state space tractable for exhaustive model checking.

## Specifications

The TLA+ specifications live in `docs/tla/` and are organized by scope:

| Specification                 | Description                                                |
| ----------------------------- | ---------------------------------------------------------- |
| `SynclineSync.tla`            | Core protocol: message relay, subscribe, sync, convergence |
| `SynclineSyncDiffLayer.tla`   | Diff layer: disk ↔ CRDT divergence, stale read bugs        |
| `SynclineSyncCrossClient.tla` | Cross-client: Obsidian vs folder client differences        |
| `SynclineSyncRename.tla`      | Rename propagation via `meta.path` updates                 |

## Verified Properties

### Safety Properties (Invariants)

These hold in **every** reachable state:

- **`TypeOK`** — All variables stay within their declared domains.
- **`NoEcho`** — The server never sends an update back to the client that originated it.
- **`PersistBeforeBroadcast`** — Updates are persisted to the database before being broadcast to other clients.
- **`IndexConsistency`** — Every document with at least one update is registered in `__index__`.
- **`ChannelOnlyForConnected`** — Only connected clients appear in broadcast channels.
- **`NoSpuriousDeletes`** — A diff that reads stale disk content never causes a remote update to be deleted.

### Liveness Properties (Temporal)

These assert that conditions _eventually_ become true, given weak fairness on server processing and client receive actions:

- **`EventualConvergence`** — If all clients are connected and no new edits occur, all clients eventually reach the same document state.
- **`EventualDelivery`** — Every update persisted on the server is eventually delivered to all subscribed clients.
- **`EventualIndexPropagation`** — Every document in `__index__` is eventually discovered by all connected clients.
- **`RenameConvergence`** — After a rename, all clients eventually agree on the new file path.
- **`DiskCrdtEventualConsistency`** — Each client's disk content eventually matches its CRDT content.

## Running the Model Checker

The specifications can be checked using [TLC](https://lamport.azurewebsites.net/tla/tools.html), the explicit-state model checker:

```bash
# Install TLA+ tools
brew install --cask tla-plus-toolbox

# Run TLC from the command line
cd docs/tla
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar \
    -config SynclineSync.cfg SynclineSync.tla

# For larger models, increase heap and use multiple workers
java -Xmx8g -jar tla2tools.jar -workers 8 \
    -config SynclineSync.cfg SynclineSync.tla
```

Typical model checking runs with 2 clients and 1 document complete in under a minute. Larger configurations (3 clients, 2 documents) may take 5–30 minutes but explore millions of distinct states.

For the full specification details, TLA+ source code, and the complete list of verified properties, see `docs/FORMAL_VERIFICATION.md` in the repository.
