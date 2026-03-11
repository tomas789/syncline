<p align="center">
  <img src="logo-with-name.jpg" alt="Syncline — Self-hosted Obsidian Sync" width="400">
</p>

# Introduction & Usage

Syncline is a self-hosted sync system for Obsidian vaults. You run the server on your own hardware, install the Obsidian plugin, and your notes stay in sync across all your devices. No subscriptions, no third-party cloud.

Under the hood it uses CRDTs (Conflict-free Replicated Data Types) to merge edits at the character level. That means you can edit the same note on two devices while both are offline for days, and when they reconnect the edits merge cleanly. No conflict dialogs, no duplicate files.

## Why Syncline?

Every other sync option either costs money monthly or can't merge concurrent text edits. Here's the short version:

|                           | Syncline | Obsidian Sync | iCloud / Dropbox |
| ------------------------- | -------- | ------------- | ---------------- |
| **You own the data**      | ✅       | ❌            | ❌               |
| **Self-hosted**           | ✅       | ❌            | ❌               |
| **Works offline**         | ✅       | ✅            | Partial          |
| **No conflict dialogs**   | ✅       | ❌            | ❌               |
| **Binary file sync**      | ✅       | ✅            | ✅               |
| **Single-file database**  | ✅       | ❌            | ❌               |
| **Subscription required** | ❌       | ✅            | ✅               |

## What You Get

**Privacy.** Notes never leave your infrastructure. The server runs on whatever you own — a home server, a $5/month VPS, a Raspberry Pi. No telemetry, no analytics, no phoning home.

**A single database file.** The entire vault history sits in one SQLite file (`syncline.db`). Backups? Copy that file. That's it.

**Speed.** The server is written in Rust and communicates over WebSockets. A keystroke on your phone shows up on your laptop in under a second.

**Real offline support.** Not the "kinda works if you reconnect within an hour" kind. Work offline for weeks. Syncline merges everything when you come back.

**Character-level merging.** Same technology as Notion and Figma. Two devices edit the same paragraph simultaneously, and the result contains both edits. No human intervention needed.

**Binary files too.** Images, PDFs, attachments — all synced. Concurrent edits to a binary file preserve both versions under distinct file names.

---

## Getting Started

### Step 1: Run the Syncline Server

The easiest way on a Mac or Windows machine is the **Syncline Desktop App**. It sits in your system tray, handles the database, and shows you the connection URL.

**Download from the [Releases Page](https://github.com/tomas789/syncline/releases):**

- **Mac:** `.dmg`
- **Windows:** `.msi` or `.nsis.zip`

It auto-starts with your computer. Click the Syncline menu bar icon, select **Start Server**, and you're up.

---

**Headless / CLI setup:**
For a Linux VPS or Raspberry Pi where there's no desktop environment:

```bash
# Download the CLI binary from Releases, or build from source:
syncline server --port 3030 --db-path ./syncline.db
```

**CLI Client:**
If you don't want to use the Obsidian plugin (maybe you're syncing a vault to a backup machine), there's a standalone headless client:

```bash
# Point the client to your vault folder and the server URL
syncline sync -f /path/to/my/vault -u ws://127.0.0.1:3030/sync
```

Both binaries are cross-platform (Linux, macOS, Windows) and have configurable log levels (`--log-level trace|debug|info|warn|error`).

### Step 2: Install the Plugin

**Option A — Community Plugins (recommended)**

1. Open Obsidian → Settings → Community Plugins
2. Search for **Syncline**
3. Install, then Enable

**Option B — Manual**

1. Download `main.js`, `manifest.json`, and `styles.css` from the [latest release](https://github.com/tomas789/syncline/releases)
2. Copy them to `<your-vault>/.obsidian/plugins/syncline-obsidian/`
3. Reload Obsidian, enable in Settings → Community Plugins

### Step 3: Connect

1. Settings → Syncline
2. Enter your server URL (e.g. `ws://192.168.1.100:3030/sync` or `wss://sync.yourdomain.com/sync`)
3. Click **Connect**

Syncing starts immediately. Install the plugin on your other devices and point them at the same server.

---

## How Does It Actually Work?

Think of it this way. Regular file sync (Dropbox, iCloud, Nextcloud) works at the file level. You change a file, I change the same file, and the system panics because it's looking at two different blobs of bytes. "Conflict! Pick one!"

Syncline doesn't sync files. It syncs individual edits.

When you type a letter, Syncline records that specific keystroke and its position. The server collects these tiny updates from all connected devices. When a client reconnects, it sends its state vector — basically a list of "I've seen edits 1 through 47 from device A, and 1 through 23 from device B." The server responds with exactly the edits the client is missing. Because CRDTs guarantee that order of application doesn't matter, the documents converge to the same state on every device, regardless of what order the updates arrived in.

It's like Git, but automatic, and it tracks individual characters instead of lines.

---

Read more about [how it works](how-it-works.md) under the hood.
