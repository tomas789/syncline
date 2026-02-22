<p align="center">
  <img src="./assets/banner.png" alt="SYNCLINE Banner" width="80%">
</p>

<p align="center">
  <strong>Vault Sync You Actually Own.</strong>
</p>

<p align="center">
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/build-passing-brightgreen.svg" alt="Build Status">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/built_with-Rust-dca282.svg?logo=rust" alt="Rust"></a>
  <a href="https://obsidian.md/"><img src="https://img.shields.io/badge/plugin-Obsidian-blueviolet.svg?logo=obsidian" alt="Obsidian Plugin"></a>
  <img src="https://img.shields.io/badge/sync-CRDTs-success.svg" alt="CRDTs">
</p>

<br/>

<p align="center">
  <b>Keep your vault in sync across all devices.</b><br>
  No subscriptions • No cloud lock-in • No third-party servers
</p>

<p align="center">
  <i>You run the server. You own the data. It <b>just works</b>.</i>
</p>

<br/>

<p align="center">
  <a href="#installation">Installation</a> •
  <a href="#key-features">Features</a> •
  <a href="#how-it-works">How It Works</a> •
  <a href="#frequently-asked-questions">FAQ</a>
</p>

<hr/>

## Why Syncline?

Most sync solutions for Obsidian ask you to trust a third party with your notes. Syncline is different:

|                           | Syncline | Obsidian Sync | iCloud / Dropbox |
| ------------------------- | -------- | ------------- | ---------------- |
| **You own the data**      | ✅       | ❌            | ❌               |
| **Self-hosted**           | ✅       | ❌            | ❌               |
| **Works offline**         | ✅       | ✅            | Partial          |
| **No conflict dialogs**   | ✅       | ❌            | ❌               |
| **Binary file sync**      | ✅       | ✅            | ✅               |
| **Single-file database**  | ✅       | ❌            | ❌               |
| **Subscription required** | ❌       | ✅            | ✅               |

---

## Key Features

### 🔒 Privacy-First

Your notes never leave your infrastructure. The Syncline server runs on hardware you control — a home server, a VPS, a Raspberry Pi. No telemetry, no analytics, no third-party access.

### 📁 Single-File Server Storage

All vault data is stored in a single SQLite database file on your server. Backup is as simple as copying one file. No complex database administration required.

### ⚡ Fast & Lightweight

Built in Rust, the server is extremely efficient. It uses WebSockets for real-time communication, meaning changes appear on your other devices within milliseconds of saving.

### ✈️ True Offline Mode

Work without an internet connection for hours, days, or weeks. When you reconnect, Syncline automatically merges all changes from all devices — no manual conflict resolution needed.

### 🤝 No Conflict Dialogs — Ever

Syncline uses **CRDTs** (Conflict-free Replicated Data Types), the same technology powering collaborative editors like Notion and Figma. When two devices edit the same note simultaneously, the changes are merged mathematically. You will never see a "conflict file" for text edits.

### 🖼️ Binary File Synchronization

Images, PDFs, attachments — all synchronized. Binary files use a content-addressed storage system. If two devices modify the same binary file while offline, both versions are preserved with distinct names so nothing is lost.

---

## How It Works

```
  Your Laptop          Your Server          Your Phone
  (Obsidian)           (Syncline)           (Obsidian)
      │                    │                    │
      │── edit note ──────►│                    │
      │                    │── push update ────►│
      │                    │                    │
      │   (go offline)     │   (go offline)     │
      │── edit note A      │                    │── edit note A
      │                    │                    │
      │   (reconnect) ────►│◄─── (reconnect) ───│
      │                    │                    │
      │◄── merged A+B ─────┤──── merged A+B ───►│
      │   (no conflicts!)  │                    │
```

The server stores a compact history of changes. When you reconnect after being offline, it sends you exactly what changed — and merges your local edits automatically.

---

## Installation

### Step 1: Run the Syncline Server

The server is a small, self-contained binary you run on any machine accessible from your devices.

**Download and run the server from the [tomas789/syncline](https://github.com/tomas789/syncline) repository:**

```bash
# Clone the repository
git clone https://github.com/tomas789/syncline
cd syncline

# Build and start the server
cargo run -- server
```

The server starts on port `3030` by default. Data is stored in `syncline.db` in the current directory.

**Options:**

```bash
cargo run -- server --port 4000 --db-path /data/my-vault.db --log-level debug
```

**Run the CLI Client:**
If you prefer to run the standalone headless CLI client rather than using the Obsidian plugin:

```bash
# Point the client to your vault folder and the server URL
cargo run -- sync -f /path/to/my/vault -u ws://127.0.0.1:3030/sync
```

Both the server and CLI client are fully cross-platform (Linux, macOS, Windows) and feature rich text output with customizable log levels (`--log-level trace|debug|info|warn|error`).

### Step 2: Install the Plugin

**Option A — Community Plugins (recommended)**

1. Open Obsidian → Settings → Community Plugins
2. Search for **Syncline**
3. Click Install, then Enable

**Option B — Manual Installation**

1. Download `main.js`, `manifest.json`, and `styles.css` from the [latest release](https://github.com/tomas789/syncline/releases)
2. Copy them to `<your-vault>/.obsidian/plugins/syncline-obsidian/`
3. Reload Obsidian and enable the plugin in Settings → Community Plugins

### Step 3: Connect

1. Open Settings → Syncline
2. Enter your server URL (e.g. `ws://192.168.1.100:3030/sync` or `wss://sync.yourdomain.com/sync`)
3. Click **Connect**

Your vault will begin syncing immediately. Install the plugin on your other devices and point them to the same server.

---

## Frequently Asked Questions

**Do I need to keep the server running all the time?**
No. The server stores all changes durably in SQLite. If the server is offline when you make edits, your changes are queued locally and synced automatically the next time the server is reachable.

**What happens if I edit the same note on two devices while offline?**
Both sets of changes are preserved and merged automatically using CRDTs. You will never lose a character you typed.

**What about images and attachments?**
Binary files are fully supported. They are synchronized using content-addressed storage. If two devices modify the same binary file concurrently, both versions are kept (with distinct names) so nothing is lost.

**Is my data encrypted?**
Data is transmitted over WebSockets. For encryption in transit, put the server behind a reverse proxy with TLS (e.g., Nginx + Let's Encrypt). End-to-end encryption support is planned.

**How do I back up my data?**
Copy the `syncline.db` file from your server. That single file contains the complete history of your vault.

**Can I use this on mobile?**
Yes. The plugin works on both desktop and mobile Obsidian.

---

## Repository

- **Main repository:** [github.com/tomas789/syncline](https://github.com/tomas789/syncline)

---

## License

MIT
