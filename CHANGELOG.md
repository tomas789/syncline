# Changelog

## [0.7.0](https://github.com/tomas789/syncline/compare/v0.6.0...v0.7.0) (2026-04-25)


### ⚠ BREAKING CHANGES

* Lagged clients now receive a full state dump instead of silently missing updates. This increases reliability but may cause a brief bandwidth spike when a slow client catches up.
* Updates that fail to persist are no longer broadcast. Connected clients will not see them until the sender retries and the DB write succeeds.
* The E2E test suite now requires the syncline binary to compile synchronously before spawning parallel processes.

### Features

* add convergence checksums for divergence detection ([2ba7cbc](https://github.com/tomas789/syncline/commit/2ba7cbcca693f29495145a41b32e316e03dd89b6))
* add periodic state vector reconciliation to detect sync divergence ([570f090](https://github.com/tomas789/syncline/commit/570f09097ae14340841b6c03d006581a0fc3c258))
* Add WASM build process and upload client, server, and WASM artifacts. ([04270c9](https://github.com/tomas789/syncline/commit/04270c907ec88ed16bb55fbc257edea422bbec87))
* **client_folder:** add --exclude CLI flag for configurable sync exclusions ([3d9ca6f](https://github.com/tomas789/syncline/commit/3d9ca6f26368c3917a2dcfde0ead9276ffebebfa))
* **desktop:** implement syncline desktop tray app with live stats and auto-updates ([bf59672](https://github.com/tomas789/syncline/commit/bf596722909ef91975ca30d31718b688686c80cd))
* implement binary file synchronization support ([064bfcd](https://github.com/tomas789/syncline/commit/064bfcd699595166624cacb846f70c4f3a1b9f25))
* merge server and client_folder into unified syncline binary ([cf66972](https://github.com/tomas789/syncline/commit/cf66972cfbb75754177c4b9e8d72f778d5f46ef8))
* New logo ([860c1ca](https://github.com/tomas789/syncline/commit/860c1ca6fa13885f2e8a5947ea4394bc32e73e46))
* **v1 client:** conservative projection → disk reconcile ([e796b01](https://github.com/tomas789/syncline/commit/e796b0166c34e7280b774ce5c6a671d5cd092358))
* **v1 client:** inbound content subdoc sync (Phase 3.3b.1) ([39ba8df](https://github.com/tomas789/syncline/commit/39ba8dfca8656d8b1dd45cf26382523b0a2fb1d2))
* **v1:** convergence verification via projection hash ([01475f7](https://github.com/tomas789/syncline/commit/01475f7f1c78b029455b711b138554925d5627a7))
* **v1:** manifest data structures, projection, and LWW types ([1b52c68](https://github.com/tomas789/syncline/commit/1b52c68bb1dd7b81410b86c66477e8b8ed130dba))
* **v1:** manifest sync protocol wire format ([ea97dca](https://github.com/tomas789/syncline/commit/ea97dca03ab3c14a52fa44b7c931540a351f66a4))
* **v1:** minimal v1 client skeleton — handshake + manifest sync ([2d4411f](https://github.com/tomas789/syncline/commit/2d4411f5da41d8cf470cb724003e118490ea88d3))
* **v1:** path-level operation handlers (create/delete/rename/modify) ([c1d251e](https://github.com/tomas789/syncline/commit/c1d251e687bf30221893d4dcc88fd772bc159758))
* **v1:** server-side DB migration from v0 to v1 schema ([0d137c3](https://github.com/tomas789/syncline/commit/0d137c32776d3489b8ececd91239bfa74692e926))
* **v1:** server-side v1 protocol handler + handshake ([8a620fd](https://github.com/tomas789/syncline/commit/8a620fd17c4de7fb286445b665ab90611045f718))
* **v1:** syncline migrate CLI + on-disk vault layout ([21c330b](https://github.com/tomas789/syncline/commit/21c330b90745c207b8cbbf03001bbac8ede630ca))
* **v1:** v0 -&gt; v1 migration over on-disk vault ([fe3b245](https://github.com/tomas789/syncline/commit/fe3b245237a0f381fad9c442b7123c07dc511db8))


### Bug Fixes

* bug 13: Propagation of empty remote file as file deletion ([5def540](https://github.com/tomas789/syncline/commit/5def540f7f39efe9f3d1c2aca77c7265fd9e118c))
* bug 14: Task leak in Network Client on reconnection ([546dd01](https://github.com/tomas789/syncline/commit/546dd01dfd49cd0a5fc3a1d25127f6a04da03c6d))
* bug 15: get_doc_id correctly handles macOS symlinked paths gracefully failing canonicalization ([eb78a8f](https://github.com/tomas789/syncline/commit/eb78a8fdff16545c338b69eaaedb09ba5d3faeae))
* bug in Arc usage by Yrs ([6613f37](https://github.com/tomas789/syncline/commit/6613f37153da9004480e215f404c802743e13342))
* **client:** scan_once respects tombstones on reconnect ([13b7843](https://github.com/tomas789/syncline/commit/13b7843076e1a09e3630c7ef4f5ee889cbc06ab2))
* close stale-read race window in Obsidian file-watcher suppression ([20f6e36](https://github.com/tomas789/syncline/commit/20f6e36971587185da89df81df0b704d782c6edb))
* enable web-sys Window feature for WASM resync interval ([92c6bfe](https://github.com/tomas789/syncline/commit/92c6bfeacf1e9787470847b58a6b91b34ec8d66f))
* make test_both_offline_same_name_conflict resilient to slow CI ([10a9d16](https://github.com/tomas789/syncline/commit/10a9d160695ad049394ee413fa230f8295b8414d))
* persist CRDT state across sessions to preserve offline edits ([d9b1dec](https://github.com/tomas789/syncline/commit/d9b1dec7a9098cf1c4e33e496a1741bec8745d23))
* **plugin:** address ObsidianReviewBot lint findings ([d8302cf](https://github.com/tomas789/syncline/commit/d8302cf456f54dc11a0a5e9d1f58c1c3046e7e5c))
* **plugin:** repair v1 text sync, surface BigInt friction, prevent borrow re-entry ([4ff59d7](https://github.com/tomas789/syncline/commit/4ff59d78bb13d06ca8f12055cc333581dc621958))
* prevent initial sync from reverting other clients' edits ([5a25365](https://github.com/tomas789/syncline/commit/5a25365b3f1f76b93630f2821c87b3dadeebee6e))
* prevent race in __index__ sync and stabilize flaky e2e tests ([7c18c0e](https://github.com/tomas789/syncline/commit/7c18c0ecae3e51f451e431b7732f8e6842a1f895))
* prevent stale file content from deleting recently typed characters ([c5fe508](https://github.com/tomas789/syncline/commit/c5fe50828e6b9d027c69e16a3c1b22b5c276d51b))
* recover from broadcast lag via full DB catch-up ([fdc3017](https://github.com/tomas789/syncline/commit/fdc30175de0f78b58c0a1ed37f366a8c1aa34e29))
* remove initial_server_uuids guard from path collision detection ([fb9f98c](https://github.com/tomas789/syncline/commit/fb9f98cddceed82ae5b47661d4814f635bbed144))
* resolve UUID-to-filename mapping in BlobUpdate handler via meta.path fallback ([52c3418](https://github.com/tomas789/syncline/commit/52c341802c7d4a088768cc82dbb0165057a38c81))
* retain pending offline changes on send failure for retry ([b134f4f](https://github.com/tomas789/syncline/commit/b134f4fdfe93968a04aceb5399b08c79834c0b60))
* skip broadcast when DB persistence fails to prevent split-brain ([e05a38b](https://github.com/tomas789/syncline/commit/e05a38b50a10f8ecb7e4772ac255208236620c87))
* **test:** replace fixed sleep with polling loop in test_filter_ignored_files ([c8671e0](https://github.com/tomas789/syncline/commit/c8671e0033b4d33e043d8c9a32d1673e45374ee1))
* **test:** replace fixed sleeps with polling loops in offline conflict test ([cdb9a4c](https://github.com/tomas789/syncline/commit/cdb9a4ceb36ce626ccad3ad7a4e1427156974cfb))
* trigger release workflow to verify artifacts ([20e918a](https://github.com/tomas789/syncline/commit/20e918a17b66014730526d2cd72beae22c26c787))
* two-tier collision detection with deterministic tie-breaker ([573074a](https://github.com/tomas789/syncline/commit/573074a70ced291962ed214126f5909263a8b6f1))
* use atomic writes with fsync for CRDT state persistence ([605c17c](https://github.com/tomas789/syncline/commit/605c17c500d5a520704d8aae7891c222fd0d1037))


### Tests

* resolve E2E compilation timeout race condition ([d879f8b](https://github.com/tomas789/syncline/commit/d879f8b500dd2f883b41f37d0407463ac02d5cbf))

## [0.6.0](https://github.com/tomas789/syncline/compare/v0.5.2...v0.6.0) (2026-04-15)


### Features

* implement binary file synchronization support ([064bfcd](https://github.com/tomas789/syncline/commit/064bfcd699595166624cacb846f70c4f3a1b9f25))
* New logo ([860c1ca](https://github.com/tomas789/syncline/commit/860c1ca6fa13885f2e8a5947ea4394bc32e73e46))


### Bug Fixes

* make test_both_offline_same_name_conflict resilient to slow CI ([10a9d16](https://github.com/tomas789/syncline/commit/10a9d160695ad049394ee413fa230f8295b8414d))
* remove initial_server_uuids guard from path collision detection ([fb9f98c](https://github.com/tomas789/syncline/commit/fb9f98cddceed82ae5b47661d4814f635bbed144))
* resolve UUID-to-filename mapping in BlobUpdate handler via meta.path fallback ([52c3418](https://github.com/tomas789/syncline/commit/52c341802c7d4a088768cc82dbb0165057a38c81))
* two-tier collision detection with deterministic tie-breaker ([573074a](https://github.com/tomas789/syncline/commit/573074a70ced291962ed214126f5909263a8b6f1))

## [0.5.2](https://github.com/tomas789/syncline/compare/v0.5.1...v0.5.2) (2026-03-11)


### Bug Fixes

* close stale-read race window in Obsidian file-watcher suppression ([5d5bc35](https://github.com/tomas789/syncline/commit/5d5bc359e1179a53b90ba82027a5f7d002695cb8))

## [0.5.1](https://github.com/tomas789/syncline/compare/v0.5.0...v0.5.1) (2026-03-11)


### Bug Fixes

* prevent stale file content from deleting recently typed characters ([72d5327](https://github.com/tomas789/syncline/commit/72d532706c791f5652d18514c61fd61edc5f26c8))

## [0.5.0](https://github.com/tomas789/syncline/compare/v0.4.1...v0.5.0) (2026-03-02)


### ⚠ BREAKING CHANGES

* The E2E test suite now requires the syncline binary to compile synchronously before spawning parallel processes.

### Tests

* resolve E2E compilation timeout race condition ([59d27bb](https://github.com/tomas789/syncline/commit/59d27bbaf717146593345281369bdcb0d6349326))

## [0.4.1](https://github.com/tomas789/syncline/compare/v0.4.0...v0.4.1) (2026-02-22)


### Bug Fixes

* trigger release workflow to verify artifacts ([a585855](https://github.com/tomas789/syncline/commit/a5858556052ac1f21e0e1666916a31fb0e524545))

## [0.4.0](https://github.com/tomas789/syncline/compare/v0.3.0...v0.4.0) (2026-02-22)


### Features

* Add WASM build process and upload client, server, and WASM artifacts. ([04270c9](https://github.com/tomas789/syncline/commit/04270c907ec88ed16bb55fbc257edea422bbec87))
* **client_folder:** add --exclude CLI flag for configurable sync exclusions ([b001791](https://github.com/tomas789/syncline/commit/b001791efcb662567b3f515462f29265a5fa16a8))
* **desktop:** implement syncline desktop tray app with live stats and auto-updates ([19ef39b](https://github.com/tomas789/syncline/commit/19ef39b110b311829fe9996a55c3da40e2335bd1))
* merge server and client_folder into unified syncline binary ([94ffdea](https://github.com/tomas789/syncline/commit/94ffdea7af65fb964549e3274e12bc93db0033ea))


### Bug Fixes

* bug 13: Propagation of empty remote file as file deletion ([5085ba6](https://github.com/tomas789/syncline/commit/5085ba627bc6443a92a969d5495dbf8d629015b2))
* bug 14: Task leak in Network Client on reconnection ([56c5cd9](https://github.com/tomas789/syncline/commit/56c5cd99fc7ba3df4fb5bd8d769a158c5c9e1b40))
* bug 15: get_doc_id correctly handles macOS symlinked paths gracefully failing canonicalization ([33cd9a6](https://github.com/tomas789/syncline/commit/33cd9a639477b8d433bb29b8b6a8850f995e9af9))
* bug in Arc usage by Yrs ([6613f37](https://github.com/tomas789/syncline/commit/6613f37153da9004480e215f404c802743e13342))
