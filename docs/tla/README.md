# Syncline TLA+ Formal Verification

Formal verification of the Syncline synchronization protocol using TLA+ and the TLC model checker.

## Quick Start

```bash
# Download TLC (one-time)
curl -L -o tla2tools.jar https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar

# Phase 1: Core protocol (all properties pass, ~30s)
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSync.cfg -workers 4 -nowarning SynclineSync.tla

# Phase 2: Diff layer — REPRODUCES Issue #6 (~1s, expected violation)
java -XX:+UseParallelGC -jar tla2tools.jar -config SynclineSyncDiffLayerBug.cfg -workers 4 -nowarning SynclineSyncDiffLayer.tla

# Phase 2: Diff layer FIXED — confirms the fix works (~14s)
java -XX:+UseParallelGC -jar tla2tools.jar -config SynclineSyncDiffLayerFixed.cfg -workers 4 -nowarning SynclineSyncDiffLayerFixed.tla

# Phase 3: Cross-client (Obsidian + folder) convergence (~37s)
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSyncCrossClient.cfg -workers 4 -nowarning SynclineSyncCrossClient.tla

# Phase 4: Multi-document + index (full with liveness, ~13s)
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSyncMultiDoc.cfg -workers 4 -nowarning SynclineSyncMultiDoc.tla

# Phase 5: Rename propagation (safety only, ~1m20s)
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSyncRenameSmall.cfg -workers 4 -nowarning SynclineSyncRename.tla

# Phase 5: Rename propagation (full with liveness, ~10min)
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSyncRename.cfg -workers 4 -nowarning SynclineSyncRename.tla
```

## Specification Files

### Phase 1: Core Protocol

| File                    | Description                                                   |
| ----------------------- | ------------------------------------------------------------- |
| `SynclineSync.tla`      | Core protocol: FIFO channels, server relay, offline/reconnect |
| `SynclineSync.cfg`      | Full config: 3 updates, queue=4, invariants + liveness (~30s) |
| `SynclineSyncSmall.cfg` | Smoke test: 2 updates, queue=3, invariants only (~1s)         |

### Phase 2: Diff Layer (Issue #6)

| File                             | Description                                                   |
| -------------------------------- | ------------------------------------------------------------- |
| `SynclineSyncDiffLayer.tla`      | Buggy model with non-atomic apply+write — reproduces Issue #6 |
| `SynclineSyncDiffLayerBug.cfg`   | Config that demonstrates the NoContentLoss violation          |
| `SynclineSyncDiffLayerFixed.tla` | Fixed model with atomic apply+write — bug eliminated          |
| `SynclineSyncDiffLayerFixed.cfg` | Config that verifies the fix holds                            |

### Phase 3: Cross-Client (Obsidian + Folder)

| File                          | Description                                                            |
| ----------------------------- | ---------------------------------------------------------------------- |
| `SynclineSyncCrossClient.tla` | Both client types in one model: Obsidian (fixed) vs folder (pre-apply) |
| `SynclineSyncCrossClient.cfg` | Config: A=Obsidian, B=Folder, safety + convergence liveness            |

### Phase 4: Multi-Document + Index

| File                            | Description                                                               |
| ------------------------------- | ------------------------------------------------------------------------- |
| `SynclineSyncMultiDoc.tla`      | Multi-doc with `__index__` document, discovery, and cross-doc convergence |
| `SynclineSyncMultiDoc.cfg`      | Full config: 2 docs, 3 updates, safety + liveness (~13s)                  |
| `SynclineSyncMultiDocSmall.cfg` | Smoke test: 2 docs, 2 updates, safety only (~1s)                          |

### Phase 5: Rename Propagation

| File                          | Description                                                                     |
| ----------------------------- | ------------------------------------------------------------------------------- |
| `SynclineSyncRename.tla`      | Models `meta.path` CRDT field, local rename, watcher detection, and propagation |
| `SynclineSyncRename.cfg`      | Full config: 2 paths, 1 update, safety + RenameConvergence liveness (~10min)    |
| `SynclineSyncRenameSmall.cfg` | Safety only: 2 paths, 1 update (~1m20s)                                         |

## Verified Properties

### Phase 1: Safety Invariants

| Property                  | Description                                  | Status  |
| ------------------------- | -------------------------------------------- | ------- |
| `TypeOK`                  | All variables stay within declared types     | ✅ Pass |
| `NoEcho`                  | Server never sends update back to originator | ✅ Pass |
| `PersistBeforeBroadcast`  | DB written before any client sees the update | ✅ Pass |
| `ChannelOnlyForConnected` | Only connected clients in broadcast channels | ✅ Pass |
| `IndexConsistency`        | Every doc with data is in the index          | ✅ Pass |
| `SubscriptionConsistency` | Client subscriptions match server channels   | ✅ Pass |

### Phase 1: Liveness Properties

| Property              | Description                                             | Status  |
| --------------------- | ------------------------------------------------------- | ------- |
| `EventualConvergence` | Connected+subscribed clients converge when queues drain | ✅ Pass |

### Phase 2: Diff Layer Properties

| Property                  | Buggy Model                | Fixed Model |
| ------------------------- | -------------------------- | ----------- |
| `NoEcho`                  | ✅ Pass                    | ✅ Pass     |
| `ChannelOnlyForConnected` | ✅ Pass                    | ✅ Pass     |
| `NoContentLoss`           | ❌ **VIOLATED** (Issue #6) | ✅ Pass     |

### Phase 3: Cross-Client Properties

| Property                  | Mixed Deployment (A=Obsidian, B=Folder) |
| ------------------------- | --------------------------------------- |
| `NoEcho`                  | ✅ Pass                                 |
| `ChannelOnlyForConnected` | ✅ Pass                                 |
| `NoContentLoss`           | ✅ Pass                                 |
| `CrossClientConvergence`  | ✅ Pass (liveness)                      |

### Phase 4: Multi-Document + Index Properties

| Property                   | Type     | Status  |
| -------------------------- | -------- | ------- |
| `IndexConsistency`         | Safety   | ✅ Pass |
| `IndexSubsetInvariant`     | Safety   | ✅ Pass |
| `NoEcho`                   | Safety   | ✅ Pass |
| `ChannelOnlyForConnected`  | Safety   | ✅ Pass |
| `NoContentLoss`            | Safety   | ✅ Pass |
| `CrossDocConvergence`      | Liveness | ✅ Pass |
| `EventualIndexPropagation` | Liveness | ✅ Pass |

### Phase 5: Rename Propagation Properties

| Property                  | Type     | Status  |
| ------------------------- | -------- | ------- |
| `NoContentLoss`           | Safety   | ✅ Pass |
| `NoEcho`                  | Safety   | ✅ Pass |
| `ChannelOnlyForConnected` | Safety   | ✅ Pass |
| `RenameConvergence`       | Liveness | ✅ Pass |

## Issue #6 Counterexample (TLC Trace)

The buggy model produces a 10-step counterexample trace:

| Step | Action                         | State Change                                      |
| ---- | ------------------------------ | ------------------------------------------------- |
| 1    | Init                           | Both clients empty                                |
| 2    | `LocalDiskEdit(A, d1)`         | A edits file → disk=`{1}`, CRDT=`{}`              |
| 3-5  | Subscribe + Sync               | Both clients subscribe, A syncs                   |
| 6    | `ClientWatcherFires(A)`        | A's watcher: diff(`{}`, `{1}`) → sends `adds={1}` |
| 7    | `ServerReceiveUpdate(A)`       | Server persists `{1}`                             |
| 8    | `ServerReceiveSyncStep1(B)`    | Server sends B `adds={1}`                         |
| 9    | `ClientApplyRemote(B)`         | B CRDT=`{1}`, **disk still=`{}`**                 |
| 10   | **`ClientWatcherFires(B)`** 💥 | Stale disk diff: `dels={1}` → **content lost!**   |

**Root cause**: Missing disk write between steps 9-10. The fix (atomic apply+write) eliminates the stale-read window.

## Verification Results

| Spec                 | States Generated | Distinct | Depth | Time  | Result                    |
| -------------------- | ---------------- | -------- | ----- | ----- | ------------------------- |
| Phase 1 Small        | 133K             | 26K      | 27    | 1s    | ✅ All pass               |
| Phase 1 Full         | 4.5M             | 860K     | 35    | 34s   | ✅ All pass               |
| Phase 2 Buggy        | 123K             | 34K      | 11    | 1s    | ❌ NoContentLoss violated |
| Phase 2 Fixed        | 10.3M            | 1.7M     | 37    | 14s   | ✅ All pass               |
| Phase 3 Cross-Client | 4.7M             | 811K     | 36    | 37s   | ✅ All pass               |
| Phase 4 Small        | 80K              | 14K      | 17    | 1s    | ✅ All pass               |
| Phase 4 Full         | 607K             | 97K      | 19    | 13s   | ✅ All pass               |
| Phase 5 Safety       | 59.8M            | 6.8M     | 104   | 1m20s | ✅ All pass               |
| Phase 5 Full         | 59.8M            | 6.8M     | 104   | 9m53s | ✅ All pass               |

## Key Insights from Phase 3

The cross-client model revealed an important correctness constraint:

**`ObsidianIgnoreExpires` must not fire before the disk write completes.** The model initially violated NoContentLoss when the ignore timer expired before `ObsidianWriteToDisk` had a chance to run. In the real code, this is guaranteed because:

1. `await vault.modify()` completes before the `setTimeout` starts
2. The 1000ms timeout far exceeds the time needed for disk I/O

The TLA+ spec encodes this via a precondition: `clientDisk[c][d] = clientDoc[c][d]` on `ObsidianIgnoreExpires`, ensuring the write-before-expire ordering.

The folder client's pre-apply strategy (read disk → apply local diff → apply remote → write atomically) is **provably safe** — it needs no `ignoreChanges` mechanism because disk always matches CRDT after processing. The folder client's pre-apply strategy remains safe across multiple documents — no cross-document interference.

### Phase 5: Rename Propagation via meta.path

The rename model confirms that the `meta.path` CRDT field correctly propagates file path changes:

- **Path is part of the CRDT**: `meta.path` is stored in a `Y.Map("meta")` alongside the `Y.Text("content")`, so renames are synchronized through the same CRDT update mechanism as content edits.
- **Watcher detection**: The two-pass rename detection (delete + create with matching content = rename) correctly updates `meta.path` and broadcasts the change.
- **Remote application**: Receiving clients atomically update both `meta.path` in the CRDT and the file location on disk, preventing inconsistencies.
- **Convergence**: `RenameConvergence` proves that after any sequence of renames and content edits, all connected/subscribed clients eventually agree on both the file path and its content.

The large state space (6.8M distinct states even with `MaxUpdates=1`) is due to the combinatorial explosion of path × content × connection states across two clients. Despite this, all 59.8M state transitions maintain safety.

## Architecture

See [FORMAL_VERIFICATION.md](../FORMAL_VERIFICATION.md) for the full design rationale, action-to-code mapping, and the roadmap for further verification (compaction, multi-doc, cross-client, rename).
