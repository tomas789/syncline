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

| Spec                 | States Generated | Distinct | Depth | Time | Result                    |
| -------------------- | ---------------- | -------- | ----- | ---- | ------------------------- |
| Phase 1 Small        | 133K             | 26K      | 27    | 1s   | ✅ All pass               |
| Phase 1 Full         | 4.5M             | 860K     | 35    | 34s  | ✅ All pass               |
| Phase 2 Buggy        | 123K             | 34K      | 11    | 1s   | ❌ NoContentLoss violated |
| Phase 2 Fixed        | 10.3M            | 1.7M     | 37    | 14s  | ✅ All pass               |
| Phase 3 Cross-Client | 4.7M             | 811K     | 36    | 37s  | ✅ All pass               |

## Key Insights from Phase 3

The cross-client model revealed an important correctness constraint:

**`ObsidianIgnoreExpires` must not fire before the disk write completes.** The model initially violated NoContentLoss when the ignore timer expired before `ObsidianWriteToDisk` had a chance to run. In the real code, this is guaranteed because:

1. `await vault.modify()` completes before the `setTimeout` starts
2. The 1000ms timeout far exceeds the time needed for disk I/O

The TLA+ spec encodes this via a precondition: `clientDisk[c][d] = clientDoc[c][d]` on `ObsidianIgnoreExpires`, ensuring the write-before-expire ordering.

The folder client's pre-apply strategy (read disk → apply local diff → apply remote → write atomically) is **provably safe** — it needs no `ignoreChanges` mechanism because disk always matches CRDT after processing.

## Architecture

See [FORMAL_VERIFICATION.md](../FORMAL_VERIFICATION.md) for the full design rationale, action-to-code mapping, and the roadmap for further verification (compaction, multi-doc, cross-client).
