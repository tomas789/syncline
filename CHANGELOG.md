# Changelog

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
