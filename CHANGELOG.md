# Changelog

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
