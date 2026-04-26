# Changelog

## [1.1.5](https://github.com/tomas789/syncline/compare/v1.1.4...v1.1.5) (2026-04-26)


### Bug Fixes

* **client:** periodic scan every 5 min, end-to-start, never overlapping ([#76](https://github.com/tomas789/syncline/issues/76)) ([6bf8a03](https://github.com/tomas789/syncline/commit/6bf8a033f44cb3f1403cfdd5a2239865b9c6dd79))

## [1.1.4](https://github.com/tomas789/syncline/compare/v1.1.3...v1.1.4) (2026-04-26)


### Bug Fixes

* **server:** cap per-doc broadcast channel at 64 to prevent OOM ([#73](https://github.com/tomas789/syncline/issues/73)) ([2c9bdb5](https://github.com/tomas789/syncline/commit/2c9bdb5436ef85ca0a380c1dd14372c09d5b3302))

## [1.1.3](https://github.com/tomas789/syncline/compare/v1.1.2...v1.1.3) (2026-04-26)


### Bug Fixes

* enable TLS in released binaries ([#54](https://github.com/tomas789/syncline/issues/54)) ([2587535](https://github.com/tomas789/syncline/commit/25875351de04c20dd8a343fa03d80647dba93bf9))
* plugin sync races, case-insensitive paths, bulk-bootstrap perf ([36ab99e](https://github.com/tomas789/syncline/commit/36ab99e059c0a00ae03b6f4f8f0022ac6e8b5712))
* throttle scanner bulk send and persist manifest before WS sends ([b4a5660](https://github.com/tomas789/syncline/commit/b4a5660ab863c7805e2577385914e172ebda08d1))

## [1.1.2](https://github.com/tomas789/syncline/compare/v1.1.1...v1.1.2) (2026-04-26)


### Bug Fixes

* **server:** drop empty STEP_2 broadcasts to stop delete-resurrection ([85ce6c5](https://github.com/tomas789/syncline/commit/85ce6c55bec81e8050e59f4e142dbf61daff62ef))

## [1.1.1](https://github.com/tomas789/syncline/compare/v1.1.0...v1.1.1) (2026-04-26)


### Bug Fixes

* **plugin:** gate blob requests on WS connection state ([b8f7273](https://github.com/tomas789/syncline/commit/b8f72737ce6dd902c108ac96326b18f1b68b82d7))
* **plugin:** make ingestNewFile idempotent for already-tracked paths ([9bd7104](https://github.com/tomas789/syncline/commit/9bd71046cbb77f4afba153d3702cc74288a8b196))
* **plugin:** stop "Folder already exists" error flood on reconcile ([0dd1145](https://github.com/tomas789/syncline/commit/0dd114575c94b271880a552a9a80080f8c797be4))
* **plugin:** treat ENOENT as benign in vault/state operations ([4de283e](https://github.com/tomas789/syncline/commit/4de283e7a0dddd4fadbc3230b69126a6c1d5a5bc))
* **sync:** reciprocate content STEP_1 to unblock client→server pushes ([70fde18](https://github.com/tomas789/syncline/commit/70fde185d5349db4cda6dd082af6c997d0db111b))

## [1.1.0](https://github.com/tomas789/syncline/compare/v1.0.1...v1.1.0) (2026-04-25)


### Features

* **cli:** sync hidden files with .synclineignore filter ([04669d7](https://github.com/tomas789/syncline/commit/04669d7626fead8681d328d02d6cccb194ab9050))


### Bug Fixes

* **client:** bump scan_once channel buffer 16 → 10000 + blocking_send backpressure ([9d145eb](https://github.com/tomas789/syncline/commit/9d145eb8ae20990f26cef2b2667820e794eebdd6))

## [1.0.1](https://github.com/tomas789/syncline/compare/v1.0.0...v1.0.1) (2026-04-25)


### Bug Fixes

* **client:** scan_once respects tombstones on reconnect ([13b7843](https://github.com/tomas789/syncline/commit/13b7843076e1a09e3630c7ef4f5ee889cbc06ab2))
* **plugin:** address ObsidianReviewBot lint findings ([d8302cf](https://github.com/tomas789/syncline/commit/d8302cf456f54dc11a0a5e9d1f58c1c3046e7e5c))

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
