# Syncline v1 — Design Document

**Status:** Proposal — release/v1 branch
**Scope:** Protocol-incompatible rewrite replacing the `path_map.json` + server-side `__index__` Y.Text with a single, replicated **manifest Y.Doc** that is the source of truth for the vault namespace.
**Non-goal:** v1 does not introduce an explicit journal / op-log (see Part II of the clean-room reference). Those arrive in v1.1.

Reference architecture: `/co-ceo/research/crdt-filesystem-design.md` (internal, clean-room rewrite). v1 implements Part I of that document.

---

## 1. Goals & Non-Goals

### 1.1 Goals

- **One source of truth for the namespace.** The set of live paths, their parents, and their rename history live in a single CRDT (the *manifest*) that every client replicates and every peer updates via Yrs updates. No JSON side-files. No server-maintained index.
- **Deterministic convergence for all fs operations.** Create, delete, rename, move, and modify must converge for every ordering of concurrent and offline operations currently exercised by the 49 tests in PR #38.
- **Fix v0's root-cause bugs by construction, not by patches.**
  - Ghost delete (empty content projected as file on peer): eliminated — deletion is a `deleted=true` LWW register on a node, not an empty text doc.
  - Stale `__index__`: eliminated — the index *is* the manifest; it cannot diverge from the per-document state.
  - Rename detection fragility: eliminated — rename is a rewrite of the `name` / `parent` fields on the existing `NodeId`; no content-hash heuristic is required.
  - Directory ops unsupported: emergent support — directories are implicit children of the `parent` field; a directory "rename" is a batch of `parent` updates in one Yrs transaction.
- **Content-addressed storage** for blobs, reused across renames, deduplicated across clients.
- **Bounded metadata growth** via knowledge-vector tombstone GC (30-day floor).
- **Local one-way migration** from v0 on-disk state to v1 manifest. Clients upgrade at their own pace; there is no server-side migration.

### 1.2 Non-Goals

- **No wire compatibility with v0.** v0 and v1 clients cannot talk to each other — they use different protocol version numbers and will refuse the handshake. Users migrate the full mesh (all clients + the server) together.
- **No explicit directory documents.** A directory is simply "a path prefix that at least one live node uses as its parent chain". The empty-directory-sync case (`test_create_empty_directory_syncs`) remains a known gap, documented as v1.1 work.
- **No journal / op-log in v1.** The manifest's LWW rules cover every fs operation in the test suite. A journal (Part II §18 of the reference) would make intent recoverable across GC and enable richer audit, but is not required for convergence; deferred to v1.1.
- **No multi-vault, no partial replication.** The manifest is small (one node per live path + tombstones), always fully replicated.
- **No E2EE in v1.** E2EE is orthogonal; it ships or does not ship per `docs/E2EE_IMPLEMENTATION.md`.

---

## 2. Architecture Overview

### 2.1 The three layers

```
┌─────────────────────────────────────────────────────────────┐
│  Manifest Y.Doc   (one per vault, fully replicated)         │
│  ─────────────                                              │
│  nodes : Y.Map<NodeId, NodeEntry>                           │
│    NodeEntry { name, parent, deleted, content_ref,          │
│                kind, blob_hash, lamport, actor }            │
│  tombstones_gc : Y.Map<NodeId, GcMeta>                      │
│  knowledge   : Y.Map<ActorId, Lamport>                      │
└─────────────────────────────────────────────────────────────┘
           │  projects to
           ▼
┌─────────────────────────────────────────────────────────────┐
│  Content subdocs  (one Y.Doc per text NodeId, lazy-loaded)  │
│  ────────────────                                           │
│  "content" : Y.Text   (the file body, as CRDT)              │
│  "meta"    : Y.Map    (mtime hints, eol, bom, etc.)         │
└─────────────────────────────────────────────────────────────┘
           │  content-addresses
           ▼
┌─────────────────────────────────────────────────────────────┐
│  CAS blob store   (SHA-256-keyed, dedup'd, lazy-fetched)    │
│  ────────────────                                           │
│  .syncline/blobs/<hash[0:2]>/<hash>                         │
└─────────────────────────────────────────────────────────────┘
```

The **manifest** owns identity and structure. Every file in the vault is a `NodeId` (UUIDv7) in `nodes`. Renames and moves mutate the `name` / `parent` fields of the existing `NodeId`; deletions set `deleted=true`. Because the `NodeId` is stable, a rename-plus-modify is trivially the two Yrs updates being merged on the same entry — the fragile content-hash detection in v0's `bootstrap_offline_changes` (see `client/state.rs`) is deleted outright.

The **content subdocs** carry the body of text files. They are addressed by `NodeId`, lazily loaded via SyncStep1 only when the local filesystem needs the file. Binary files do not allocate a content subdoc — they use `content_ref = Blob(hash)`.

The **CAS blob store** is content-addressed (SHA-256). Blobs are pushed via `MSG_BLOB_UPDATE` and fetched on demand via `MSG_BLOB_REQUEST`. Binary renames cost zero bytes of blob traffic — the manifest updates, the hash does not.

### 2.2 Node lifecycle

```
                create                rename/move                delete
  (absent) ──────────────▶  live  ──────────────────▶  live  ──────────▶  tombstoned
                             │          (same NodeId)                        │
                             │                                               │
                             │    modify (new Yrs update on content subdoc)  │
                             │◀──────────────────────────────────────────────┘
                                        (resurrection — see §6.3)
```

A deletion never removes an entry from `nodes`; it sets `deleted=true` with a Lamport timestamp. Concurrent modification of a tombstoned node **resurrects it** (modify-wins-over-delete — rationale in §6.3), provided the modification's Lamport is newer. Post-GC, resurrection is no longer possible (§8).

### 2.3 Why a single manifest instead of per-file docs + a server index

v0 kept each file's content in its own Y.Doc and reconstructed the vault namespace two ways: a client-side `path_map.json` (plain JSON, not a CRDT — it de-syncs under concurrency) and a server-side `__index__` Y.Text (a CRDT, but fed by ad-hoc client code that forgets to prune on delete). Every bug listed in `docs/KNOWN_BUGS.md` #9–#15 traces to one of these two diverging.

A single manifest Y.Doc collapses both into one replicated structure. Yrs' own update-and-sync machinery guarantees eventual consistency; no side code can de-sync it because there is no side code. The server becomes a **dumb pipe + persistence layer** for the manifest and content subdocs — it no longer maintains an independent view of the namespace.

---

## 3. Data Format

### 3.1 On-disk layout (per client)

```
<vault>/
├── .syncline/
│   ├── version                       # ASCII "1\n"; tripwire for old clients
│   ├── manifest.bin                  # latest compacted Yrs snapshot of the manifest doc
│   ├── manifest.updates              # append-only log of Yrs updates since last snapshot
│   ├── content/
│   │   ├── <nodeid[0:2]>/
│   │   │   └── <nodeid>.bin          # compacted snapshot of one content subdoc
│   ├── blobs/
│   │   ├── <hash[0:2]>/
│   │   │   └── <hash>                # raw bytes of a binary file content
│   ├── actor_id                      # stable random UUIDv4 for this replica
│   └── lamport                       # monotonic per-actor counter, persisted
└── <user-visible files and directories>
```

The `.syncline/` folder is the only on-disk state the client writes. The `version` file is read before anything else; a mismatch (`!= "1"`) triggers migration (§7) or a hard refusal.

The `actor_id` is generated once, persisted, never changes. Lamport counters increment on every operation this client performs and are persisted to `lamport` after each transaction.

### 3.2 Manifest schema

```rust
type NodeId   = Uuid;       // UUIDv7, so IDs sort temporally
type ActorId  = Uuid;       // UUIDv4, one per installation
type Lamport  = u64;        // per-actor monotonic counter

struct NodeEntry {
    name:         String,           // LWW register — last segment of path
    parent:       Option<NodeId>,   // LWW register — None = vault root
    deleted:      bool,             // LWW register — tombstone flag
    kind:         NodeKind,         // Text | Binary (immutable after create)
    blob_hash:    Option<[u8; 32]>, // LWW register, only meaningful if kind == Binary
    size:         u64,              // LWW register, hint for UI / GC
    lamport:      Lamport,          // of the most recent field update on this entry
    actor:        ActorId,          // of the most recent field update on this entry
    created_at:   Lamport,          // of the create event (for GC age floor)
}

struct GcMeta {
    tombstoned_at: Lamport,   // when deleted=true was set
    last_knowledge_merge: Lamport,  // see §8
}
```

Yrs expresses each field as a value in the `NodeEntry` Y.Map; LWW resolution happens at read time by comparing `(lamport, actor)` tuples. `(lamport, actor)` with max lamport wins; ties broken by `actor` lexicographic order (stable and symmetric across peers).

### 3.3 Content subdoc schema

```
content : Y.Text           # the file body
meta    : Y.Map {
    eol:  "lf" | "crlf",   # preserved across platforms
    bom:  bool,
    lang: Option<String>,  # hint only, not load-bearing
}
```

One subdoc per text `NodeId`. Loaded on demand (see §4.3). Binary nodes never allocate a content subdoc.

### 3.4 Wire format (protocol framing)

The v0 framing `[msg_type: u8][doc_id_len: u16 BE][doc_id: utf8][payload]` is retained. v1 adds a protocol version handshake at the head of the WebSocket session:

```
client → server:  [MSG_VERSION=0xF0][u8 major=1][u8 minor=0]
server → client:  [MSG_VERSION=0xF0][u8 major=1][u8 minor=0]
                  OR close(code=1002, "protocol version mismatch")
```

The handshake happens before any doc subscription. Mismatched major versions fatally close the connection; matching majors proceed. The server will accept `(major=1, minor=*)` for the v1 lifetime.

---

## 4. Sync Protocol Changes

### 4.1 New message types

| Code   | Name                   | Direction | Payload                                                                      |
|--------|------------------------|-----------|------------------------------------------------------------------------------|
| `0xF0` | `MSG_VERSION`          | both      | `[u8 major][u8 minor]`                                                       |
| `0x20` | `MSG_MANIFEST_SYNC`    | both      | `[Yrs SyncMessage bytes]` — applies to the manifest doc only                 |
| `0x21` | `MSG_MANIFEST_VERIFY`  | both      | `[root_merkle_hash: 32 bytes][mode: u8][detail...]` — convergence heartbeat  |

Retained from v0 (unchanged semantics, except "`doc_id` = manifest" is no longer a special case):

| Code   | Name                | Notes                                                       |
|--------|---------------------|-------------------------------------------------------------|
| `0x00` | `SYNC_STEP_1`       | Sent per content subdoc (one per NodeId that is resident)   |
| `0x01` | `SYNC_STEP_2`       | Response to SYNC_STEP_1                                     |
| `0x02` | `UPDATE`            | Ordinary Yrs update on a content subdoc                     |
| `0x04` | `BLOB_UPDATE`       | CAS blob push (server persists to `blobs` table)            |
| `0x05` | `BLOB_REQUEST`      | CAS blob fetch by hash                                      |

Removed:

- `MSG_RESYNC` (0x06) and `MSG_CHECKSUM` (0x07) — replaced by `MSG_MANIFEST_VERIFY` which verifies the *namespace*, not per-doc text. Per-doc divergence is detected and healed by the ordinary SyncStep1/2 exchange on demand.

### 4.2 Session bring-up

```
1. WebSocket TCP + upgrade
2. ─▶ MSG_VERSION(1,0)           (client)
3. ◀─ MSG_VERSION(1,0)           (server)       [or disconnect]
4. ─▶ MSG_MANIFEST_SYNC(SyncStep1, state_vector = client's)
5. ◀─ MSG_MANIFEST_SYNC(SyncStep2, missing updates)
6. ◀─ MSG_MANIFEST_SYNC(SyncStep1, state_vector = server's)   [reverse direction]
7. ─▶ MSG_MANIFEST_SYNC(SyncStep2, missing updates)
── manifest now converged ──
8. (lazy) per-file: ─▶ SYNC_STEP_1 on content subdoc when needed locally
```

Manifest sync happens **first and always**, before any content subdoc is loaded. This guarantees the client sees the authoritative namespace before it makes local filesystem decisions (e.g. "does this path exist on disk? do I need to write it?").

### 4.3 Lazy content subdoc loading

v0 subscribed to every doc on connect. v1 subscribes only to content subdocs corresponding to live (non-deleted) NodeIds whose content is (a) on disk and out of sync, or (b) newly requested by the local file watcher.

Server side: the broadcast channel map `doc_id → Sender` is already per-doc; no change beyond accepting subscribes that arrive post-connect.

### 4.4 Convergence verification (`MSG_MANIFEST_VERIFY`)

Every 30 seconds a client sends a verification message:

```
MSG_MANIFEST_VERIFY, mode=0 (ROOT)
  root_hash = SHA256 of the canonical serialization of the projected namespace
              (see §4.4.1) — computed over live nodes only
```

Server responds with its own `root_hash` over the same projection. On mismatch:

```
MSG_MANIFEST_VERIFY, mode=1 (FULL_SYNC_REQUEST)
```

…which triggers an unconditional SyncStep1 of the manifest in both directions. This is the heartbeat that catches silent data loss / divergence bugs; it is cheap (one SHA256 over ~a few hundred bytes per node) and runs whenever the connection is alive.

#### 4.4.1 Canonical projection hash

```
For each live NodeId (deleted=false), in UUIDv7 ascending order:
    SHA256.update(nodeid_bytes || name_utf8 || parent_bytes_or_zero || kind_byte || blob_hash_or_zero)
Finalize.
```

Tombstones are excluded — they diverge legitimately during GC windows. A separate mode (FULL_SYNC_REQUEST, already in the table) covers the case where tombstone state is suspected of diverging.

---

## 5. Operation Semantics

All operations below are expressed as Yrs transactions on the manifest (and, for modify, also on a content subdoc). Each transaction stamps every changed register with `(lamport, actor)` where `lamport` is this client's counter, incremented once per transaction.

### 5.1 Create

**Trigger:** `notify` watcher reports a new path on disk.

```
1. Assign a fresh NodeId (UUIDv7 → temporal order)
2. Detect collision: is there a live node whose (parent, name) equals the new one?
   - Yes, and it's a path already owned by us → no-op (this is the round-trip echo)
   - Yes, but it's someone else's node → this is a "simultaneous create same path"
     → create our node anyway; at read/projection time, the later creator gets a
       conflict suffix (see §6.4). This matches v0's detect_path_collision behavior.
3. Compute SHA256 of file bytes
   - Text: start a content subdoc, init content Y.Text with the bytes
   - Binary: push blob via MSG_BLOB_UPDATE; record blob_hash in the NodeEntry
4. Insert NodeEntry { name, parent, deleted=false, kind, blob_hash, lamport, actor }
5. Broadcast MSG_MANIFEST_SYNC(UPDATE) + (if text) MSG_UPDATE on the new content subdoc
```

### 5.2 Delete

**Trigger:** `notify` watcher reports a path removal.

```
1. Find the live NodeId that projects to this path.
2. Set deleted = true; bump (lamport, actor).
3. Broadcast manifest update.
4. Do NOT touch the content subdoc or the blob immediately — GC handles that.
```

This is the direct fix for `test_delete_*` family. No more "empty text = deleted" projection hack. A `deleted=true` node vanishes from every peer's filesystem projection the moment they apply the manifest update.

### 5.3 Rename (same parent)

**Trigger:** `notify` reports a `rename(from, to)` event OR a delete-then-create on the same parent within a debounce window where the content hash matches.

```
1. Resolve `from` → NodeId via the live projection.
2. Update NodeEntry.name = new_last_segment; bump lamport/actor.
3. Broadcast manifest update. The content subdoc and the blob are untouched.
```

Note: v0's content-hash rename detection *for fs events* is retained as a fallback — `notify` on Linux sometimes delivers rename as `Remove+Create`. But the CRDT-level effect is always a single `name` field update; content hashing only disambiguates which `Remove+Create` pair to collapse on the watcher side before it reaches the manifest layer.

### 5.4 Move (different parent)

```
1. Resolve `from` → NodeId.
2. Detect collision at destination (same rule as create).
3. Update NodeEntry.parent = new_parent_nodeid; optionally .name too; bump lamport/actor.
4. Broadcast manifest update.
```

### 5.5 Modify

**Trigger:** `notify` reports a write.

```
Text:
  1. Resolve path → NodeId.
  2. Diff local text vs subdoc content → apply as Yrs Y.Text deltas.
  3. Broadcast MSG_UPDATE on the content subdoc.
  4. Bump NodeEntry.lamport (marks "this node was touched by actor", used by GC).

Binary:
  1. Resolve path → NodeId.
  2. SHA256 the new bytes. If hash matches current blob_hash → no-op.
  3. MSG_BLOB_UPDATE the new blob (CAS — server dedupes if seen).
  4. Update NodeEntry.blob_hash; bump lamport/actor.
  5. Broadcast manifest update.
```

### 5.6 Directory operations (emergent)

Syncline v1 has **no explicit directory document**. All directory ops are expressed via the `parent` field of leaves:

| User action                   | Manifest effect                                                          |
|-------------------------------|--------------------------------------------------------------------------|
| `mkdir foo/`                  | No manifest change — `foo/` only materializes when a file gets `parent = foo`. |
| `mkdir foo/; touch foo/a.md`  | Create `a.md` with a synthesized "directory node" `foo` as its parent.   |
| `mv foo/ bar/`                | One transaction: for every child with parent=foo_id, either rename foo→bar OR update each child's `parent` to point at the new directory node. v1 takes the **directory-node rename** path (one update, not N). |
| `rm -rf foo/`                 | One transaction: set `deleted=true` on the directory node AND every descendant. |

The directory node itself is a `NodeEntry { kind: Directory, blob_hash: None, ... }`. It has no content subdoc and no blob. Its only job is to carry the `name` field that the leaves reference as `parent`. This keeps directory rename O(1) in manifest updates, which matters for `test_rename_directory_with_files` (the v0 failure mode was N independent rename-detection heuristics, each racing against the others).

**Known gap:** `test_create_empty_directory_syncs` — a directory node is *created* when a child appears, not when `mkdir` runs. An empty directory therefore does not sync. This is an intentional v1 scope cut; fix is in v1.1 alongside the journal work.

### 5.7 Lamport advance rules

```
lamport_new = max(local_counter, observed_remote_lamport + 1)
```

On **every** manifest or subdoc update received, bump the local counter to at least `remote + 1`. On every local transaction, increment by one. This makes the counter a proper Lamport clock and gives LWW a well-defined order even across disconnected actors.

---

## 6. Conflict Resolution Rules

All rules operate on the projection of the manifest to "what files exist on this disk, and what do they contain". LWW is the fallback; specific op pairs have named rules that are more intuitive.

### 6.1 Field-level LWW

For any single register (`name`, `parent`, `deleted`, `blob_hash`):

```
winner = argmax_(lamport, actor) over all field writes
```

This is the Yrs default for `Y.Map`; we inherit it. Tie-break on `actor` is stable across peers because actor UUIDs are globally unique.

### 6.2 Concurrent rename to different names (`test_rename_vs_rename_different_names`)

Both writes hit the same `name` register on the same NodeId → §6.1 picks one. The losing name is **not** resurrected as a conflict copy — the user only saw one name; showing the loser as an extra file would be noise. The authoritative outcome: one name wins, the original path is gone on both sides, no conflict file appears.

### 6.3 Modify-wins-over-delete (`test_delete_vs_modify_offline_modification_wins`)

```
If a node has a write to `deleted=true` AND a subdoc content update (or blob_hash change)
with higher lamport than the delete:
   deleted := false   (the modification resurrects the node)
```

This is a projection rule, not a CRDT rule — both writes stand in the manifest; projection chooses "live" because the modification's lamport beats the delete's. Rationale: accidental or conflicting deletes in a shared vault should never destroy user edits. The user can always re-delete intentionally.

### 6.4 Same-path collisions (create vs create, modify after remote create, etc.)

If, at projection time, two live nodes share the same `(parent, name)`:

```
sorted = nodes.sort_by(lamport, actor)      # deterministic
winner = sorted[0]
for loser in sorted[1:]:
    loser.projected_name := basename + ".conflict-<short_actor>-<lamport>" + ext
```

Both peers arrive at identical projected filenames because the sort is deterministic. This covers: `test_simultaneous_online_create_same_path`, `test_both_offline_same_name_conflict`, and the binary case `test_concurrent_binary_edits_preserves_both` (the two nodes have different `blob_hash`; both survive, both are written to disk under distinct names).

### 6.5 Move + delete on same node

Treated as two independent register updates. LWW on `deleted` dominates — if the delete has the higher lamport, the move is a no-op at projection; if the move is newer, §6.3 resurrects.

### 6.6 Cycle prevention

A move that would make a node its own ancestor is **rejected locally** before being applied to the manifest. If, by network race, a cycle is ever projected, the loop is broken at the edge with the lowest `(lamport, actor)` — that edge's `parent` is reverted to the pre-edit value. This is a purely local repair; the next manifest update broadcast will correct the peers.

The Kleppmann-Howard tree CRDT would handle this without a local repair, but its extra machinery (per-op undo/redo log) is out of scope for v1. The local-repair path is sufficient for the test suite and for real-world fs hierarchies where cycles are vanishingly rare (filesystem move loops usually require adversarial timing).

---

## 7. Migration v0 → v1 (local, one-way)

### 7.1 Trigger

On client startup:

```
if .syncline/version missing or == "0":
    run migration
else if == "1":
    proceed normally
else:
    abort with "downgrade not supported"
```

### 7.2 Steps

1. **Take a backup.** Rename `.syncline/` to `.syncline.v0.bak/`. Never delete user data.
2. **Fresh-init a new `.syncline/` with `version = 1`**, a new random `actor_id`, `lamport = 0`.
3. **Walk the user-visible vault, ignoring `.syncline*`.** For every file found:
   - Assign a fresh NodeId (UUIDv7).
   - Classify as Text or Binary by the same mime-sniff rule v0 uses.
   - Text → create a content subdoc, load file bytes into the `content` Y.Text.
   - Binary → SHA256 the bytes, write to CAS blob store, record `blob_hash`.
   - Insert NodeEntry with `parent` derived from the path structure (creating directory nodes as needed).
4. **Connect to the server.** The server will either:
   - Already know us as a v1 client (we've connected before) → ordinary manifest sync catches us up.
   - Not recognize us → our manifest *is* the initial state; standard bidirectional sync handles the merge with other peers' manifests.
5. **Server migration is independent.** The server has no `path_map.json`; it has the SQLite `updates` table. A v1 server treats any doc_id that is not the manifest as a legacy content subdoc and keeps serving it. New subdocs created under v1 follow the same schema. There is no schema change needed to SQLite.

### 7.3 Safety

- Migration is **idempotent within a client**: running it twice is a no-op because step 1 is skipped if `.syncline.v0.bak` already exists, and step 2 is skipped if `version = 1`.
- Migration is **destructive to v0 sync state only**. The user's files on disk are never touched. `.syncline.v0.bak` is retained for manual rollback; users can delete it after confirming v1 works.
- There is no "mixed mesh" phase. The `MSG_VERSION` handshake refuses v0 clients against a v1 server and vice versa. Deployments must flip atomically (or run two servers side-by-side during rollout, which is a user-space choice, not a protocol concern).

### 7.4 What is not migrated

- **v0 conflict files.** Any pre-existing files named `*.conflict-*` are re-imported as ordinary files. v1 does not try to parse or collapse them.
- **v0 LWW blob history.** Only the *current* bytes of each binary are imported. Old blob versions from the v0 `blobs` SQLite table are dropped during server-side data sweep (documented operator task; not automatic).

---

## 8. Tombstone GC (Knowledge-Vector Protocol)

### 8.1 Problem

Tombstones in `nodes` (entries with `deleted=true`) grow unbounded if never removed. Removing them too early causes "zombie resurrection": a reconnecting client that still has the pre-delete state re-broadcasts it as new, and the network accepts it because the tombstone is gone.

### 8.2 Protocol

Each client maintains a **knowledge vector** `knowledge : Y.Map<ActorId, Lamport>` inside the manifest — the max lamport this client has seen from every actor it has ever observed. On every sync round (step 7 of §4.2), both peers merge their vectors.

```
For each actor A in the union of our and peer's vectors:
    knowledge[A] := max(ours[A], peer's[A])
```

The minimum across all actors of `knowledge[A]` is the **global low-water-mark** (LWM). Any tombstone whose `tombstoned_at < LWM - SAFETY` is safe to GC: every actor has seen at least up to LWM, so no one holds pre-delete state for that tombstone.

### 8.3 Parameters

- **SAFETY = 30 days** in wall-clock equivalent. Because Lamport ≠ wall clock, we track tombstone age with a separate wall-clock stamp at `tombstoned_at` creation (stored inside `GcMeta`), and apply whichever floor is stricter: `lamport < LWM - lamport_safety` AND `wall_clock_age > 30 days`.
- **Run on server** (authoritative), pushed to clients as a GC manifest update. Clients never GC unilaterally — they would race with reconnecting peers whose knowledge they don't have.

### 8.4 What GC does

For each GC-eligible tombstone:

1. Emit a manifest update that deletes the `NodeEntry` from `nodes` entirely (not just `deleted=true` — the whole entry).
2. Delete the corresponding content subdoc snapshot from `.syncline/content/`.
3. Delete the blob from `.syncline/blobs/` **only if** no other live node references the same `blob_hash` (trivial refcount over `nodes`).

### 8.5 Test-suite interaction

The test suite does not exercise multi-day tombstone GC directly. What it does exercise — deletion propagation after hours/seconds, not after 30 days — is covered by the tombstone-present (`deleted=true`) rules in §5.2 and §6.3.

---

## 9. Test Plan

All 49 tests in PR #38 (`feat/fs-operations-test-coverage`) must pass on v1. Below, each test is mapped to the v1 mechanism responsible for its outcome.

### 9.1 `fs_file_operations.rs` (18 tests)

| Test                                                          | v1 mechanism                                             |
|---------------------------------------------------------------|----------------------------------------------------------|
| create/modify/delete online & offline (6 tests)               | §5.1 / §5.5 / §5.2                                       |
| `test_delete_binary_file_online_removes_on_peer`              | §5.2 — `deleted=true` on NodeEntry; projection hides it  |
| `test_delete_removes_from_index`                              | §2.3 — no separate index; projection *is* the index      |
| `test_rapid_create_then_delete_does_not_leak_to_peer`         | Watcher debounce + §5.2 (last-write wins on `deleted`)   |
| rename text/binary, online/offline (4 tests)                  | §5.3                                                     |
| `test_rename_empty_file_preserves_identity`                   | §5.3 — NodeId stable across rename; no content-hash needed |
| `test_move_file_across_directories`                           | §5.4                                                     |
| `test_modify_while_other_deletes_offline`                     | §6.3 modify-wins-over-delete                             |
| remaining file-op variants (2 tests)                          | §5.1–§5.5                                                |

### 9.2 `fs_directory_operations.rs` (7 tests)

| Test                                                          | v1 mechanism                                             |
|---------------------------------------------------------------|----------------------------------------------------------|
| `test_create_empty_directory_syncs`                           | **Known gap** — documented in §1.2 & §5.6; test will remain ignored or asserts the gap explicitly |
| `test_create_directory_with_files_syncs`                      | §5.6 — directory node created when first child lands    |
| `test_delete_directory_removes_all_files_on_peer`             | §5.6 — one transaction tombstones dir + all descendants |
| `test_empty_directory_cleaned_after_all_files_deleted`        | §5.6 — directory node tombstoned when last child dies   |
| `test_rename_directory_with_files`                            | §5.6 — O(1) `name` update on directory node             |
| `test_move_directory_to_new_parent`                           | §5.6 — `parent` update on directory node                |
| `test_deep_nested_directory_creation`                         | §5.6 — directory nodes created chain-wise               |

### 9.3 `fs_metadata_operations.rs` (5 tests)

| Test                                                          | v1 mechanism                                             |
|---------------------------------------------------------------|----------------------------------------------------------|
| hidden-file filter                                            | Watcher-level ignore (unchanged from v0)                 |
| permission propagation                                        | **Known gap** — §1.2; not in scope for v1                |
| readonly / touch-no-op                                        | §5.5 — hash-unchanged modify is a no-op                  |

### 9.4 `fs_edge_cases.rs` (15 tests)

| Test                                                          | v1 mechanism                                             |
|---------------------------------------------------------------|----------------------------------------------------------|
| unicode / emoji / special-char filenames                      | `name` is a UTF-8 String; no v0-style path escaping      |
| long filenames (200-char)                                     | No length limit in manifest; OS-level limits apply       |
| 1MB text file                                                 | Y.Text handles it; SyncStep2 sends the full update       |
| symlinks / hard links                                         | Followed / treated as content copy (watcher level)       |
| case variants (e.g. `foo.md` vs `FOO.md` on case-sensitive FS)| Distinct NodeIds, distinct names in manifest             |
| `test_delete_then_recreate_same_name_different_content`       | New NodeId on recreate; old is tombstoned (§5.2)         |
| `test_identical_content_different_file_not_mistaken_for_rename` | NodeIds are assigned independently; identical content coincidence is harmless |

### 9.5 `fs_conflict_scenarios.rs` (6 tests)

| Test                                                          | v1 mechanism                                             |
|---------------------------------------------------------------|----------------------------------------------------------|
| `test_delete_vs_modify_offline_modification_wins`             | §6.3                                                     |
| `test_rename_vs_rename_different_names`                       | §6.2                                                     |
| `test_concurrent_binary_edits_preserves_both`                 | §6.4 — two NodeIds, both live, projected with conflict suffixes |
| `test_three_client_fanout`                                    | Ordinary manifest broadcast                              |
| `test_delete_propagates_to_late_reconnecting_client`          | §5.2 + manifest SyncStep2 on reconnect                   |
| `test_simultaneous_online_create_same_path`                   | §6.4                                                     |

### 9.6 New convergence tests introduced in v1

Beyond the PR #38 suite, v1 adds:

1. `test_manifest_verify_detects_divergence` — inject a manual desync, assert `MSG_MANIFEST_VERIFY` flags it within 30s.
2. `test_migration_v0_to_v1_preserves_all_files` — populate a v0 vault, run migration, assert every file and its bytes are present in the v1 manifest projection.
3. `test_tombstone_not_gc_before_lwm` — simulate an offline actor, assert its tombstone is retained until the LWM advances.
4. `test_cycle_move_rejected_locally` — attempt `mv foo/ foo/bar/`, assert local rejection (not a panic, not a silent corruption).

These are added in a follow-up test commit alongside the implementation.

---

## 10. Open questions (to resolve during implementation)

1. **Directory node creation policy.** Strict "materialize on first child" (current spec) vs. eager "materialize on `mkdir`". Eager would unlock `test_create_empty_directory_syncs` at the cost of watcher churn. Decision deferred to the directory-ops implementation commit.
2. **Manifest compaction cadence.** When to rewrite `manifest.bin` from `manifest.updates`. Proposal: every 1000 updates or every 10 MB of log, whichever comes first.
3. **Cross-actor clock skew.** Wall-clock tombstone safety relies on NTP sync. Users with grossly skewed clocks could delay GC by days. Acceptable; documented operator limitation.
4. **Blob GC on server.** The SQLite `blobs` table grows without bound. Server-side refcount from manifest projections is the obvious answer; cost/complexity tradeoff to assess during server work.

---

## 11. Implementation order (commit-level plan)

1. **Data structures** — `NodeId`, `NodeEntry`, `Manifest` wrapper with Yrs bindings; no wire integration yet. Unit tests for LWW and projection.
2. **Migration** — v0-layout → v1-manifest, pure function over a fixture vault; unit tests.
3. **Manifest sync protocol** — `MSG_VERSION`, `MSG_MANIFEST_SYNC` wire-up client+server; integration test `test_two_clients_manifest_converges`.
4. **Operation handlers** — wire the watcher to §5.1–§5.6 transactions.
5. **Convergence verification** — `MSG_MANIFEST_VERIFY` + the injection test.
6. **Test-suite green** — iterate until the 49 PR #38 tests pass.
7. **Tombstone GC** — server-side; add the LWM test.

Each step ships as an independent commit (or small series) on `release/v1`. Intermediate states may fail some PR #38 tests; that is expected. The branch target is "all 49 pass" before `release/v1` is proposed for merge into `main`.
