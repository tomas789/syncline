# Syncline Project Overview

Syncline is a privacy-first, self-hosted synchronization system for Obsidian vaults. It uses **Conflict-free Replicated Data Types (CRDTs)** to ensure seamless, conflict-free merging of notes across multiple devices, even after extended offline periods.

## Architecture

The project is structured as a Rust workspace with multiple components:

- **`syncline`**: The shared core library. It contains the primary CRDT logic using `yrs` and is compiled to WebAssembly (Wasm) for use in the Obsidian plugin.
- **`server`**: A high-performance synchronization server built with Rust, Axum (WebSockets), and SQLite. It stores binary CRDT updates and handles client synchronization.
- **`client_folder`**: A standalone, headless CLI client written in Rust. It monitors a local directory and synchronizes it with a Syncline server.
- **`obsidian-plugin`**: The Obsidian integration (TypeScript). It loads the `syncline` Wasm module to provide real-time, conflict-free sync within the Obsidian UI.

## Technology Stack

- **Language**: Rust (Server, CLI, Core), TypeScript (Plugin)
- **CRDT Library**: `yrs` (Rust) / `yjs` compatible
- **Networking**: WebSockets (Axum/Tokio)
- **Database**: SQLite (SQLx)
- **Build Tools**: Cargo, npm, rollup, wasm-pack

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (latest stable)
- [Node.js & npm](https://nodejs.org/)
- [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/) (for building the plugin)

### Building and Running

#### 1. Start the Server

```bash
cargo run -p server -- --port 3030 --db-path syncline.db
```

#### 2. Run the CLI Client

```bash
cargo run -p client_folder -- -f /path/to/vault -u ws://localhost:3030/sync
```

#### 3. Build the Obsidian Plugin

```bash
cd obsidian-plugin
npm install
npm run build
```

The build process includes compiling the Rust core to Wasm via `wasm-pack`.

## Development Conventions

- **CRDT First**: All text synchronization MUST go through the `yrs` CRDT logic. Never perform manual text merging.
- **Safety**: Prefer Rust for performance-critical or shared logic.
- **Testing**:
  - **Bug Fixes**: Every time you are asked to fix a bug, you MUST create a failing test case to verify your hypothesis and reproduce the issue before applying the fix.
  - Rust tests: `cargo test`
  - Integration tests: See `tests/` and `client_folder/tests/`
  - Fuzzing: Use the `fuzzer` crate to test CRDT edge cases.
- **Logging**: Use `tracing` in Rust components. The log level can be controlled via `RUST_LOG` or CLI flags.
- **Releases and Versioning**:
  - The project uses `release-please` for automated version bumps and changelog generation.
  - You MUST write commit messages following the **Conventional Commits** format (e.g., `feat: added feature`, `fix: resolving bug`, `docs: update readme`, `build: setup action`).
  - When conventional commits are pushed to the `main` branch, a Release PR is automatically created/updated. Merging that PR publishes the GitHub release and syncs versions across the `syncline`, `server`, `client_folder`, and `obsidian-plugin` components.

## Key Files

- `docs/DESIGN_DOC.md`: Detailed architectural design and rationale.
- `docs/PROTOCOL.md`: Specification of the synchronization protocol.
- `syncline/src/lib.rs`: The Wasm bridge and core client logic.
- `server/src/server.rs`: WebSocket handling and SQLite persistence.
- `obsidian-plugin/main.ts`: Main entry point for the Obsidian plugin.
