# Syncline testing playbook

This document is the long-form testing reference for Syncline. Hand
it to a new engineer (or a future LLM) and they should be able to
exercise every layer of the system - the Rust library, the server,
the CLI client, the Obsidian plugin (via wdio), and the on-disk /
in-database state - without first having to grep their way through
the source tree.

It is not a tutorial. It is an exhaustive map: for every component,
how do you start it, how do you observe it, how do you tear it
down, and how do you assert against it.

---

## 1. The system in one minute

Three processes, one CRDT, one append-only DB, one CAS:

```
   Obsidian (plugin: WASM)         CLI sync (native)
        │                                 │
        │ MSG_VERSION + MSG_MANIFEST_*    │
        │ MSG_SYNC_STEP_1/2 + MSG_UPDATE  │
        │ MSG_BLOB_REQUEST/UPDATE         │
        ▼                                 ▼
                  syncline server (axum + tokio)
                                ▼
                        SQLite: updates, blobs
```

- **Manifest** (`__manifest__`): one Yrs Y.Doc per vault listing
  every node (file/dir) by stable `NodeId`. Owns the vault's
  namespace.
- **Content subdocs** (`content:<NodeId>`): one Yrs Y.Doc per text
  file, holding a `Y.Text` named `text`.
- **Blobs**: SHA-256-keyed binary chunks (FastCDC-split for files
  > 1 MiB; small files are a length-1 chunk list).

Reference docs in the repo:
- `docs/DESIGN_DOC_V1.md` - protocol + LWW rules.
- `docs/PROTOCOL.md` - wire format.
- `docs/KNOWN_BUGS.md` - historical fault list (v0 era).

---

## 2. Library tests (Rust, no I/O or in-process subprocess)

Three layers, all under `cargo test -p syncline`:

| File / module                              | Coverage                                            |
| ------------------------------------------ | --------------------------------------------------- |
| `src/v1/*` `#[cfg(test)] mod tests`        | Per-module unit tests (manifest, projection, ops, sync, ids, chunker, …) |
| `src/client_v1.rs` `tests` module          | ContentStore disk roundtrips, reconcile, watcher batching, conflict-sibling recogniser |
| `src/server/server.rs` `tests` module      | WS handshake, manifest sync, blob upload (uses `axum::serve` on an ephemeral port) |
| `tests/v1_protocol_e2e.rs`                 | Bidirectional manifest sync between two `Manifest`s in-process; verify heartbeat |
| `tests/v1_robustness.rs`                   | Concurrent rename/delete/modify scenarios, fuzz, conflict suffixes, lamport overflow, build_path is_live, etc. |
| `tests/e2e.rs`                             | Subprocess-based: spawns real `syncline server` + `syncline sync` binaries via tokio::process |

Run patterns:

```bash
# Whole crate (slow because of e2e):
cargo test -p syncline

# Just the lib units (~5s):
cargo test -p syncline --lib

# Just one integration test file:
cargo test -p syncline --test v1_robustness
cargo test -p syncline --test v1_protocol_e2e
cargo test -p syncline --test e2e
cargo test -p syncline --test e2e test_obsidian_like_onboarding

# Module filter:
cargo test -p syncline --lib v1::projection
cargo test -p syncline --lib client_v1::tests::reconcile_

# Single test:
cargo test -p syncline --test v1_robustness directory_resurrected
```

`tests/e2e.rs` uses an atomic counter for ports starting at 18000 -
each test gets a fresh port. The other test files don't touch the
network.

### Adding a manifest-CRDT scenario

Pure-portable scenarios (no fs, no network) belong in
`tests/v1_robustness.rs`. There's an `Xs` PRNG + `random_op` helper
and a `full_mesh_sync` helper for N-peer convergence. Pattern:

```rust
let mut a = Manifest::new(ActorId::new());
let id = create_text(&mut a, "f.md", 0)?;
let mut b = Manifest::from_update(
    ActorId::new(), a.lamport(), &a.encode_state_as_update(),
)?;

a.set_name(id, "renamed.md");
delete_path(&mut b, "f.md")?;

sync(&mut a, &mut b);
assert_converged("rename-vs-delete", &[&a, &b]);
```

`assert_converged` compares `projection_hash`. For per-path
assertions, use `project(m).by_path.contains_key("…")`.

### Adding a subprocess-based scenario

Subprocess scenarios go in `tests/e2e.rs`. Helpers:

```rust
let env = TestEnv::new(2).await;          // server + 2 CLI clients
fs::write(env.client_path(0).join("foo.md"), "hi")?;
assert!(wait_for_convergence(&env.dirs(), Duration::from_secs(10)).await);
```

`TestEnv` builds the workspace, picks a port, spawns
`target/debug/syncline server`, then N `syncline sync` clients into
TempDirs. `Drop` order matters - tokio's `Child::kill_on_drop` does
the cleanup.

`compare_directories` walks every non-`.syncline` file under each
client folder and asserts identical bytes plus identical Yrs state
(it only reads `.syncline/data` if v0 layout exists; in v1 the
on-disk file content is the source of truth).

---

## 3. Running the server manually

```bash
cargo build --release --bin syncline
./target/release/syncline server \
    --port 3030 \
    --db-path ./syncline.db \
    --log-level info        # error / warn / info / debug / trace
```

What to expect on stdout (info-level):

```
🚀 Starting Syncline server...
🔌 Port: 3030
💾 Database: ./syncline.db
Migrated server DB to v1: 0 text + 0 binary + 0 skipped (actor <uuid>)
Server listening on 0.0.0.0:3030
v1 handshake OK (peer 1.0)            # one line per client connect
```

To redirect logs to a file rotate-friendly format, pass
`--log-file /var/log/syncline.log`. The CLI uses
`tracing-subscriber` + `tracing-appender::never` rotation.

To kill cleanly, send `SIGINT` (Ctrl-C). The axum `serve` task
catches it and shuts down.

---

## 4. Running the CLI client manually

```bash
./target/release/syncline sync \
    --folder /path/to/vault \
    --url ws://127.0.0.1:3030/sync \
    --name laptop \              # optional friendly tag for conflict files
    --log-level info             # try debug for protocol detail
```

First connect prints a banner and:
```
📂 Folder: /path/to/vault
🌐 Server URL: ws://127.0.0.1:3030/sync
Migrated local vault: N text, M binary, K directories
v1 handshake OK (server 1.0)
Starting debounced watcher on directory: "/path/to/vault"
```

`Ctrl-C` stops both the watcher and the WS connection.

Useful env knobs (also work in tests):
- `RUST_LOG=syncline=debug,info` overrides `--log-level`.
- `SYNCLINE_URL=…` is read by the CLI as the default for `--url`.

### What the CLI writes on disk

```
<vault>/
├── .syncline/
│   ├── version              # "1\n"
│   ├── actor_id             # UUIDv4 for this peer (stable across restarts)
│   ├── manifest.bin         # encoded Yrs update of the manifest doc
│   ├── content/             # per-text-file subdoc snapshots
│   │   └── ab/              # sharded by first 2 hex of NodeId
│   │       └── <NodeId>.bin
│   └── blobs/               # CAS — hex SHA-256 keyed
│       └── ab/cd/<sha256>
└── <user-visible files>
```

Inspect manifest from the host without going through Obsidian:

```bash
# Decode `.syncline/manifest.bin` with a small Rust shim, OR
# point the WASM-free `syncline verify` at it (next section).
```

---

## 5. `syncline verify`

```bash
./target/release/syncline verify \
    --folder /path/to/vault \
    --url ws://127.0.0.1:3030/sync \
    --timeout-secs 5
echo $?  # 0 = converged, 1 = diverged
```

Sends a single `MSG_MANIFEST_VERIFY` (the SHA-256 of the projection)
and reads the server's reply. Silence past the timeout = converged.

This is the cheapest convergence check you can run in CI / scripts -
no full state download.

---

## 6. Observing what's actually in the SQLite DB

The server stores everything in two tables:

```sql
-- Append-only log of yrs updates, keyed by doc_id.
CREATE TABLE updates (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_id      TEXT NOT NULL,            -- '__manifest__' or 'content:<NodeId>'
    update_data BLOB NOT NULL             -- yrs::Update::encode_v1()
);

-- Content-addressed blob store for binary chunks.
CREATE TABLE blobs (
    hash       TEXT PRIMARY KEY,          -- 64-char lowercase SHA-256 hex
    data       BLOB NOT NULL,
    size       INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

### sqlite3 cheat sheet

```bash
sqlite3 ./syncline.db
sqlite> .headers on
sqlite> .mode column

-- How many docs do we have?
sqlite> SELECT doc_id, COUNT(*) AS updates, SUM(LENGTH(update_data)) AS bytes
        FROM updates GROUP BY doc_id ORDER BY bytes DESC LIMIT 20;

-- Manifest bytes (one row per manifest update applied):
sqlite> SELECT id, LENGTH(update_data) FROM updates
        WHERE doc_id = '__manifest__' ORDER BY id;

-- All content subdoc IDs (= NodeIds):
sqlite> SELECT DISTINCT substr(doc_id, 9) AS node_id
        FROM updates WHERE doc_id LIKE 'content:%';

-- Blob inventory:
sqlite> SELECT COUNT(*) AS n, SUM(size) AS total_bytes,
               SUM(size)/1024/1024 AS total_mib FROM blobs;
sqlite> SELECT hash, size FROM blobs ORDER BY size DESC LIMIT 10;

-- Find blobs not referenced by any manifest entry: (manual; needs
-- decoding the manifest BLOBs above with the syncline binary or a
-- small yrs-aware script — there is no SQL-only way).
```

### Decoding a yrs blob from SQL

The `updates.update_data` column is a `yrs::Update` v1 binary. To
inspect it, write a tiny Rust binary or Node script that:

1. `let update = yrs::Update::decode_v1(bytes)?;`
2. `let doc = yrs::Doc::new();`
3. `doc.transact_mut().apply_update(update);`
4. For the manifest, walk `doc.get_or_insert_map("nodes")`.
5. For a content subdoc, read `doc.get_or_insert_text("text").get_string(&txn)`.

The cleanest in-tree way: temporarily add a `#[test]` to
`server::tests` that takes the bytes from a fixture file and prints
the decoded structure. For one-off ops, the WASM client's
`update_content_text` / `manifestSnapshot` round-trips via the
plugin sidebar.

### Verifying a SQL state matches a peer's disk

Two ways:

1. Run `syncline verify` from a peer's vault against the server.
   Identical projection hash → SQLite ↔ peer disk agree on the
   namespace. This does **not** prove per-file content agreement;
   for that, force a `MSG_SYNC_STEP_1` per content subdoc (the WASM
   client's "reconnect" button, or restart `syncline sync`).
2. Pipe `compare_directories` over two CLI peers' folders. If both
   show identical SHAs and the server hasn't broadcast since, the
   server's SQLite is the same as both.

### Resetting / wiping a server DB

```bash
rm -f /path/to/syncline.db /path/to/syncline.db-shm /path/to/syncline.db-wal
```

Server is stateless beyond this DB. Restart and clients will push
their state on first connect.

---

## 7. Driving the Obsidian plugin manually

### Build

```bash
cd obsidian-plugin
npm ci
npm run build       # = wasm-pack build + rollup
```

Outputs:
- `obsidian-plugin/main.js` (compiled plugin)
- `obsidian-plugin/manifest.json`
- `obsidian-plugin/wasm/syncline_bg.wasm` (~480 KB)

### Install into a vault

```bash
mkdir -p /path/to/vault/.obsidian/plugins/syncline
cp obsidian-plugin/{main.js,manifest.json,styles.css} \
   /path/to/vault/.obsidian/plugins/syncline/
cp -r obsidian-plugin/wasm /path/to/vault/.obsidian/plugins/syncline/
```

Open Obsidian → Settings → Community Plugins → enable "Syncline
(live sync)". Configure server URL in the Syncline settings panel
and click Connect.

### Inspect plugin state at runtime

In Obsidian's developer console (`Ctrl-Shift-I`):

```js
// Raw plugin handle:
const p = app.plugins.plugins['syncline'];

// Are we connected?
p.client.isConnected();

// Manifest snapshot (for diffing):
new Uint8Array(p.client.manifestSnapshot()).slice(0, 32);

// Per-node projection (matches `projection_json()`):
JSON.parse(p.client.projectionJson()).slice(0, 5);

// Bytes of a content subdoc by node id:
new Uint8Array(p.client.contentSnapshot('<uuid>'));

// Force a verify heartbeat:
p.client.sendVerify(); p.client.lastVerifyResult();

// Settings (configSync categories etc.):
p.settings.configSync;
```

### configSync defaults

`DEFAULT_CONFIG_SYNC` in `obsidian-plugin/main.ts`:

| Field                  | Default | What it controls                                   |
| ---------------------- | ------- | -------------------------------------------------- |
| `themes`               | true    | `.obsidian/themes/`                                |
| `snippets`             | true    | `.obsidian/snippets/`                              |
| `hotkeys`              | true    | `.obsidian/hotkeys.json`                           |
| `coreConfig`           | true    | `app.json`, `appearance.json`, `core-plugins.json` |
| `communityPluginList`  | false   | `.obsidian/community-plugins.json`                 |
| `communityPluginData`  | `{}`    | Per-plugin opt-in: `{ '<id>': true }`              |
| `other`                | false   | Anything under `.obsidian/` not covered above      |

`.obsidian/workspace.json`, `.obsidian/workspace-mobile.json`,
`.obsidian/cache/`, `.obsidian/graph.json` are **always ignored** -
device-local Obsidian state.

### Reload the plugin's manifest list (after Syncline drops new
plugins on disk)

```js
await app.plugins.loadManifests();
console.log(Object.keys(app.plugins.manifests).filter(id => id !== 'syncline'));
```

This is what the wdio cross-vault spec uses to assert "the new
plugin is now installed" on vault B.

---

## 8. wdio (Obsidian) e2e

Real Obsidian, real chromedriver, drives the plugin end-to-end.

### Layout

```
e2e/
├── wdio.conf.ts                        # service config
├── test/specs/*.e2e.ts                 # the specs
├── test-vault/                         # default vault (gitignored)
├── .obsidian-cache/                    # downloaded Obsidian + chromedriver
└── docker/                             # podman/docker harness (§9)
```

Key existing specs:

| Spec                                | What it covers                                                |
| ----------------------------------- | ------------------------------------------------------------- |
| `basic.e2e.ts`                      | Empty vault + empty CLI → roundtrips Obsidian↔CLI             |
| `phase1/2/3/4.e2e.ts`               | Big-vault rsync + cross-host (claw.krej.ci)                   |
| `issue56/57/58/63/90/91/93/94/95.e2e.ts` | Per-bug regressions                                       |
| `pre-existing-vault.e2e.ts`         | Vault has notes BEFORE plugin enabled                         |
| `community-plugin-sync.e2e.ts`      | configSync.communityPluginData[<id>] opt-in                   |
| `plugin-cross-vault-install.e2e.ts` | Install in vault A → reloadObsidian into vault B → see it     |

### Run

```bash
cd e2e
npm ci
PATH=/usr/bin:$PATH npm test                                  # all
PATH=/usr/bin:$PATH npm test -- --spec test/specs/basic.e2e.ts  # one
```

(`PATH=/usr/bin:$PATH` is in the package.json `test` script - it
prevents brew's `mocha` from shadowing wdio's bundled mocha
worker.)

### Driving Obsidian from a spec

`browser.executeObsidian` runs a closure inside the Obsidian
renderer process. Pass primitives in/out only - functions don't
serialise:

```ts
const id = await browser.executeObsidian(async ({ app }, path) => {
    const f = app.vault.getAbstractFileByPath(path);
    return f ? f.path : null;
}, 'note.md');
```

Common ops:

| Action                         | Snippet                                                       |
| ------------------------------ | ------------------------------------------------------------- |
| Read a vault file (text)       | `await app.vault.read(file)`                                  |
| Read a vault file (binary)     | `await app.vault.readBinary(file)`                            |
| Write a vault file             | `await app.vault.modify(file, body)`                          |
| Create a vault file            | `await app.vault.create(path, body)`                          |
| Rename via Obsidian's API      | `await app.fileManager.renameFile(file, newPath)`             |
| Get the vault's root path      | `app.vault.adapter.basePath`                                  |
| List installed plugin manifests| `app.plugins.manifests`                                       |
| Trigger plugin rescan          | `await app.plugins.loadManifests()`                           |
| Enable/disable Syncline        | `await app.plugins.enablePlugin('syncline')`                  |

The wdio service also exposes an `obsidianPage` helper:

```ts
const obsidianPage = browser.getObsidianPage();
await obsidianPage.write('test.md', '# hi');     // create / overwrite
await obsidianPage.enablePlugin('syncline');
await obsidianPage.resetVault();                  // reset to original vault
await browser.reloadObsidian({ vault, plugins }); // reboot on a different vault
```

### Polling helper

Every spec defines a `waitFor(label, fn, timeoutMs, stepMs)` because
sync is asynchronous. Use generous timeouts on first-bootstrap waits
(content has to stream through WS).

### Debugging a wdio failure

- The spec output streams `[chrome] ...` lines from Obsidian's
  console.
- Add `console.log` inside `executeObsidian` closures - they tee to
  stderr.
- Re-run with `--logLevel debug` to see every webdriver round-trip
  (very chatty).
- The spec's `before()` hook usually prints the syncline binary's
  stdout via `cliProc.stdout?.on('data', ...)`. Keep that.
- Examine the spec's CLI sync folder (`<spec>-cli/`) and the
  in-flight `.syncline/manifest.bin` to confirm the manifest matches
  expectations.

---

## 9. Containerised tests (podman / docker)

`e2e/docker/` packages all of §8 into a Debian 12 image with Xvfb,
xauth, p7zip (for AppImage extraction), the Rust toolchain, and the
electron/chromium runtime libs.

```bash
# One-time:
podman compose -f e2e/docker/docker-compose.yml build

# Run a spec:
podman compose -f e2e/docker/docker-compose.yml run --rm e2e \
  bash e2e/docker/run-tests.sh \
  --spec test/specs/plugin-cross-vault-install.e2e.ts
```

Same flags work for `docker compose`. See `e2e/docker/README.md`
for the full doc, including SELinux relabel + arm64 vs amd64 notes.

What runs inside the container:
1. `cargo build --release --bin syncline`
2. `obsidian-plugin/`: `npm ci` + `npm run build`
3. `e2e/`: `npm ci` + `xvfb-run npx wdio run wdio.conf.ts`

Artifacts persist across runs via named volumes
(`cargo-cache`, `cargo-target`, `obsidian-cache`,
`obsidian-plugin-node-modules`, `e2e-node-modules`).

---

## 10. Cross-cutting test scenarios worth knowing about

These are scenarios that catch a class of regressions; mention them
to anyone designing new features.

| Scenario                                              | Where it lives                                          |
| ----------------------------------------------------- | ------------------------------------------------------- |
| Cycle via concurrent moves                            | `v1_robustness::concurrent_moves_into_each_other_*`     |
| Concurrent rename of same node                        | `v1_robustness::concurrent_rename_of_same_node_*`       |
| Rename + delete race                                  | `v1_robustness::concurrent_rename_and_delete_*`         |
| Modify-wins-over-delete (file)                        | `v1_robustness::rename_after_observed_delete_*`         |
| Modify-wins-over-delete (directory)                   | `v1_robustness::directory_resurrected_via_modify_wins_*`|
| Same-path collision (binary)                          | `v1_robustness::binary_file_same_path_collision_*`      |
| 4-peer same-path collision                            | `v1_robustness::four_peer_same_path_collision_*`        |
| Conflict suffix uniqueness across many peers          | `v1_robustness::conflict_suffixes_are_pairwise_distinct`|
| Lamport overflow at u64::MAX                          | `v1::ids::tests::lamport_observe_does_not_overflow_*`   |
| Empty binary file (size=0) materialises               | `client_v1::tests::reconcile_materialises_truly_empty_*`|
| File/directory same-name collision in projection      | `client_v1::tests::reconcile_does_not_bail_when_*`      |
| Two-leading-dots filename in conflict path            | `client_v1::tests::conflict_sibling_path_double_dot_*`  |
| v0→v1 migration with malformed paths                  | `v1::migration::tests::migrate_drops_paths_with_*`      |
| Pre-existing vault joins server with CLI peer         | `e2e::test_obsidian_like_onboarding_*`                  |
| Real Obsidian with pre-existing notes                 | `e2e/test/specs/pre-existing-vault.e2e.ts`              |
| Real Obsidian community-plugin opt-in                 | `e2e/test/specs/community-plugin-sync.e2e.ts`           |
| Cross-vault plugin install via Syncline               | `e2e/test/specs/plugin-cross-vault-install.e2e.ts`      |

---

## 11. Multi-peer scenarios

For interactive bug repro, spin up:

```bash
# Terminal 1: server
./target/release/syncline server --port 3030 --db-path /tmp/syncline.db

# Terminal 2: peer A
mkdir -p /tmp/vault-a && \
  ./target/release/syncline sync \
    -f /tmp/vault-a -u ws://127.0.0.1:3030/sync --name peer-a

# Terminal 3: peer B
mkdir -p /tmp/vault-b && \
  ./target/release/syncline sync \
    -f /tmp/vault-b -u ws://127.0.0.1:3030/sync --name peer-b

# Terminal 4: drive edits
echo "hi" > /tmp/vault-a/note.md
ls /tmp/vault-b              # should show note.md within ~3s
```

Network partition repro: kill the server (`kill <pid>`), do edits
on both peers, restart the server. They reconnect, push their
diffs, projection_hash converges within a couple of seconds.

Three peers + concurrent ops: `tests/v1_robustness.rs ::
three_peers_rename_modify_delete_converge` is the in-process
analogue. For an over-the-wire variant, just spawn a third
`syncline sync` against the same server.

---

## 12. Known caveats and gotchas

- **Case-insensitive filesystems** (macOS APFS, NTFS): two notes
  named `Foo.md` and `foo.md` collide on disk. Syncline preserves
  both NodeIds, but only one materialises. Documented as an open
  issue.
- **`.obsidian/community-plugins.json`** is **not** synced by
  default - it's `communityPluginList: false`. Toggle it on
  explicitly if you want the plugin enable-list to propagate.
- **Self-exclusion**: the Syncline plugin never syncs its own
  `${configDir}/plugins/syncline/` directory - that holds device-
  local state (server URL, actor id).
- **fsync**: the client skips fsync per file in bulk bootstrap.
  Recovery is via re-sync from the server, not durable per-file
  writes. If a CLI peer crashes mid-bootstrap, deleting `.syncline/`
  and reconnecting is the recommended fix.
- **AppImage in containers**: requires `7z` (no FUSE in the
  container). The provided Dockerfile ships `p7zip-full`.
- **Electron sandbox**: wdio-obsidian-service auto-injects
  `--no-sandbox` on Linux. Don't add it again or you'll get
  duplicate-flag warnings.

---

## 13. The "I just want to verify a fix" checklist

When you land a change, the standard chain is:

1. `cargo test -p syncline --lib` (5 s) - unit tests.
2. `cargo test -p syncline --test v1_robustness` (~1 s) - manifest
   scenarios.
3. `cargo test -p syncline --test v1_protocol_e2e` (~0.1 s) -
   protocol roundtrips.
4. `cargo test -p syncline --test e2e` (~2 min) - subprocess
   integration.
5. `cd obsidian-plugin && npm run build` - rebuild the WASM bundle
   so wdio specs run against current source.
6. `cd e2e && PATH=/usr/bin:$PATH npm test -- --spec test/specs/<your-spec>.e2e.ts`
   for whichever Obsidian path you touched.
7. Optional: `podman compose -f e2e/docker/docker-compose.yml run --rm e2e`
   to confirm the container build still boots.

If steps 1-4 are green and the relevant wdio spec is green, you're
in good shape.
