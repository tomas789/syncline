# Formal Verification

Syncline's synchronization protocol is formally verified using [TLA+](https://lamport.azurewebsites.net/tla/tla.html), a specification language designed by Leslie Lamport for modeling concurrent and distributed systems.

## Why Bother?

The Yjs/Yrs CRDT library is independently proven to converge. The tricky part is everything *around* it: message ordering, server relay, broadcast channels, offline reconnection, the diff layer, the index document. Lots of places for subtle bugs to hide.

Testing catches many of those. But testing only explores specific execution traces. TLA+ exhaustively checks **all possible interleavings**, including rare race conditions you'd never trigger in a test suite. Several real bugs were first predicted by the TLA+ model before showing up in the code:

| Risk area             | Bug                                                                                             |
| --------------------- | ----------------------------------------------------------------------------------------------- |
| **Message relay**     | Server echoes updates back to the sender → infinite feedback loop (Issue #2)                    |
| **Channel lifecycle** | New doc updates are dropped if no broadcast channel exists yet (fuzzer-found divergence bug)    |
| **Diff layer**        | Stale file content fed into `dissimilar::diff` generates DELETE ops for valid data (Issue #6)   |
| **Client divergence** | Obsidian's `ignoreChanges` timeout vs folder client's sync-before-apply lead to different races |
| **Index document**    | Concurrent insertions into `__index__` from multiple connections corrupt the doc list           |

## What's Verified

**In scope:**

- All clients eventually converge to the same document state
- Updates aren't echoed back to the sender
- Every update eventually reaches all connected clients
- All accepted updates are persisted before broadcast
- The `__index__` document accurately reflects all known documents
- Stale file reads fed into the differ don't cause data loss
- Both Obsidian and folder clients reach the same state under identical scenarios
- After a rename, all clients agree on the new path

**Out of scope (verified by other means):**

- Yrs CRDT merge correctness (proven by the Yjs/YATA literature)
- Binary encoding correctness (unit tests)
- WebSocket transport reliability (handled by the transport layer)
- SQLite transactional guarantees (handled by SQLite)

## System Model

The TLA+ spec models Syncline as a distributed system with a three-tier client architecture: the CRDT document, the file on disk, and the diff layer bridging them.

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

CRDTs are abstracted as opaque update tokens. Two documents count as "converged" when they've applied the exact same set of updates, regardless of order. The CRDT guarantees order-independence, so this abstraction keeps the state space small enough for exhaustive model checking.

## Specifications

The TLA+ specs live in `docs/tla/`, organized by scope:

| Specification                 | What it covers                                             |
| ----------------------------- | ---------------------------------------------------------- |
| `SynclineSync.tla`            | Core protocol: message relay, subscribe, sync, convergence |
| `SynclineSyncDiffLayer.tla`   | Diff layer: disk ↔ CRDT divergence, stale read bugs        |
| `SynclineSyncCrossClient.tla` | Cross-client: Obsidian vs folder client differences        |
| `SynclineSyncRename.tla`      | Rename propagation via `meta.path` updates                 |

## Verified Properties

### Safety (Invariants)

These hold in every reachable state:

- **TypeOK** — All variables stay within their declared domains
- **NoEcho** — The server never sends an update back to the originating client
- **PersistBeforeBroadcast** — Updates are persisted to the database before broadcast
- **IndexConsistency** — Every document with at least one update appears in `__index__`
- **ChannelOnlyForConnected** — Only connected clients appear in broadcast channels
- **NoSpuriousDeletes** — A diff from stale disk content never deletes a valid remote update

### Liveness (Temporal)

These assert that conditions eventually become true, assuming weak fairness on server processing and client receive actions:

- **EventualConvergence** — If all clients are connected and no new edits occur, all clients reach the same state
- **EventualDelivery** — Every persisted update is eventually delivered to all subscribed clients
- **EventualIndexPropagation** — Every document in `__index__` is eventually discovered by all connected clients
- **RenameConvergence** — After a rename, all clients eventually agree on the new path
- **DiskCrdtEventualConsistency** — Each client's disk content eventually matches its CRDT content

## Running the Model Checker

Use [TLC](https://lamport.azurewebsites.net/tla/tools.html), the explicit-state model checker:

```bash
# Install TLA+ tools
brew install --cask tla-plus-toolbox

# Run TLC
cd docs/tla
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar \
    -config SynclineSync.cfg SynclineSync.tla

# For larger models, increase heap and use multiple workers
java -Xmx8g -jar tla2tools.jar -workers 8 \
    -config SynclineSync.cfg SynclineSync.tla
```

With 2 clients and 1 document, model checking finishes in under a minute. Larger configs (3 clients, 2 documents) can take 5–30 minutes, exploring millions of distinct states.

Full specification source and the complete list of verified properties are in `docs/FORMAL_VERIFICATION.md`.
