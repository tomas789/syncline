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

# Phase 6: Path collision — REPRODUCES bug (~1s, expected violation)
# To see the bug, revert the collision detection to require `d \in initialServerDocs[c]`
# java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSyncPathCollisionSmall.cfg -workers 4 -nowarning SynclineSyncPathCollision.tla

# Phase 6: Path collision FIXED — safety only (~3min)
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSyncPathCollisionSmall.cfg -workers 4 -nowarning SynclineSyncPathCollision.tla

# Phase 6: Path collision FIXED — full with liveness (~19min)
java -XX:+UseParallelGC -Xmx4g -jar tla2tools.jar -config SynclineSyncPathCollision.cfg -workers 4 -nowarning SynclineSyncPathCollision.tla
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

### Phase 6: Path Collision

| File                                    | Description                                                                            |
| --------------------------------------- | -------------------------------------------------------------------------------------- |
| `SynclineSyncPathCollision.tla`         | Two offline clients create same file; models collision detection + server-wins resolve  |
| `SynclineSyncPathCollision.cfg`         | Full config: safety + CollisionConvergence liveness (~19min)                            |
| `SynclineSyncPathCollisionSmall.cfg`    | Safety only (~3min)                                                                    |

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

### Phase 6: Path Collision Properties

| Property                  | Buggy Model                             | Fixed Model |
| ------------------------- | --------------------------------------- | ----------- |
| `NoPathDuplication`       | ❌ **VIOLATED** (11-step counterexample) | ✅ Pass     |
| `NoContentLoss`           | ❌ (not reached)                         | ✅ Pass     |
| `ChannelOnlyForConnected` | ❌ (not reached)                         | ✅ Pass     |
| `CollisionConvergence`    | ❌ (not reached)                         | ✅ Pass     |

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

## Path Collision Counterexample (TLC Trace)

The buggy collision model produces an 11-step counterexample:

| Step | Action                       | Key State                                                |
| ---- | ---------------------------- | -------------------------------------------------------- |
| 1    | Init                         | Both clients offline                                     |
| 2    | `CreateDocOffline(A, d1)`    | A creates `file.md` → UUID=d1                           |
| 3    | `CreateDocOffline(B, d2)`    | B creates `file.md` → UUID=d2                           |
| 4    | `ClientGoOnline(A)`          | A connects; `initialServerDocs[A] = {}`                  |
| 5    | `ClientGoOnline(B)`          | B connects; `initialServerDocs[B] = {}` (d1 not yet!)    |
| 6-7  | Upload + register            | A uploads d1 → server registers d1                       |
| 8-9  | Discover + sync              | B discovers d1 via index, sends SyncStep1                |
| 10   | Server responds              | Server sends SyncStep2: d1, path=`"file.md"`             |
| 11   | **`ClientApplyRemote(B)`** 💥 | Collision NOT detected: `d1 ∉ initialServerDocs[B]`     |

**Root cause**: `initial_server_uuids` is set from the first `__index__` response. If B connects before A uploads, B's snapshot is empty. The guard `d ∈ initialServerDocs` prevents collision detection for any doc that appeared after B connected.

**Fix**: Remove the `initialServerDocs` guard entirely. Check path_map directly: any incoming doc whose path collides with a freshly-created local doc triggers resolution. Verified in Rust code (`app.rs:detect_path_collision`) and TLA+.

## Verification Results

| Spec                      | States Generated | Distinct | Depth | Time    | Result                        |
| ------------------------- | ---------------- | -------- | ----- | ------- | ----------------------------- |
| Phase 1 Small             | 133K             | 26K      | 27    | 1s      | ✅ All pass                   |
| Phase 1 Full              | 4.5M             | 860K     | 35    | 34s     | ✅ All pass                   |
| Phase 2 Buggy             | 123K             | 34K      | 11    | 1s      | ❌ NoContentLoss violated     |
| Phase 2 Fixed             | 10.3M            | 1.7M     | 37    | 14s     | ✅ All pass                   |
| Phase 3 Cross-Client      | 4.7M             | 811K     | 36    | 37s     | ✅ All pass                   |
| Phase 4 Small             | 80K              | 14K      | 17    | 1s      | ✅ All pass                   |
| Phase 4 Full              | 607K             | 97K      | 19    | 13s     | ✅ All pass                   |
| Phase 5 Safety            | 59.8M            | 6.8M     | 104   | 1m20s   | ✅ All pass                   |
| Phase 5 Full              | 59.8M            | 6.8M     | 104   | 9m53s   | ✅ All pass                   |
| Phase 6 Buggy             | 7.6K             | 2.9K     | 12    | < 1s    | ❌ NoPathDuplication violated |
| Phase 6 Safety (fixed)    | 87.4M            | 12.4M    | 48    | 2m50s   | ✅ All pass                   |
| Phase 6 Full (fixed)      | 87.4M            | 12.4M    | 48    | 18m47s  | ✅ All pass                   |

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

### Phase 6: Path Collision — Bug Found and Fixed

This phase discovered a **real bug** in the collision detection logic of `app.rs`. The TLA+ model found a `NoPathDuplication` violation in just 11 steps:

- **The race**: When both clients connect before either uploads, `initial_server_uuids` is empty on both. A doc uploaded by client A after client B has already connected is never in B's snapshot, so the collision guard `d ∈ initialServerDocs` always returns false.
- **The fix**: Remove the `initialServerDocs` guard entirely. Check the path_map directly: if an incoming doc's `meta.path` collides with a locally freshly-created doc's path, trigger resolution regardless of timing.
- **Second bug found during fix**: The server echoes back the doc's original `meta.path` in a SyncStep2 response after the client has already resolved the conflict. This would revert the conflict path. Fixed by keeping the local path when the client already has the doc with a non-null path.
- **Verification**: After both fixes, 87.4M states explored with 12.4M distinct, depth 48. All safety invariants and `CollisionConvergence` liveness pass.

## Architecture

See [FORMAL_VERIFICATION.md](../FORMAL_VERIFICATION.md) for the full design rationale, action-to-code mapping, and the roadmap for further verification (compaction, multi-doc, cross-client, rename).

