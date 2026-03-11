<p align="center">
  <img src="./docs/logo-with-name.jpg" alt="Syncline — Self-hosted Obsidian Sync" width="400">
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
  <a href="#features">Features</a> •
  <a href="#how-it-works">How It Works</a> •
  <a href="#faq">FAQ</a>
</p>

---

## Why Syncline?

Every other sync option for Obsidian requires you to hand your notes to someone else's servers. Obsidian Sync costs $8/month. iCloud and Dropbox can't merge text edits, so you end up with `Note (conflict copy 2).md` scattered across your vault.

Syncline runs on your own machine. It stores everything in a single SQLite file. And it uses CRDTs to merge edits at the character level, so two people can edit the same paragraph offline for a week and the result still comes out clean.

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

## Features

**Privacy-first.** Your notes stay on your infrastructure. A home server, a VPS, a Raspberry Pi sitting in a closet. No telemetry, no analytics.

**Single-file storage.** The entire vault history lives in one `syncline.db` file. Back it up by copying that file. Done.

**Fast.** Written in Rust. Sync happens over WebSockets, so changes show up on other devices in milliseconds.

**Offline for as long as you want.** Go off-grid for weeks. When you reconnect, Syncline merges everything automatically. No manual conflict resolution, ever.

**Character-level merging.** Built on CRDTs (the same tech behind Notion and Figma's collaborative editing). Two devices editing the same note at the same time? The edits merge mathematically. You won't see a "conflict file."

**Binary files too.** Images, PDFs, attachments — all synced. If two devices modify the same binary file offline, both versions are kept with distinct names.

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

The server keeps a compact history of changes. When you reconnect after being offline, it sends you only what changed and merges your local edits automatically.

---

## Installation

### Step 1: Run the Syncline Server

The quickest option on Mac or Windows is the **Syncline Desktop App**. It sits in your system tray, manages the database, and gives you the connection URL.

**Grab the installer from the [Releases Page](https://github.com/tomas789/syncline/releases):**

- **Mac:** `.dmg`
- **Windows:** `.msi` or `.nsis.zip`

It starts with your computer. Click the menu bar icon → **Start Server**.

---

**CLI / headless setup:**
For a Linux VPS or Raspberry Pi where there's no GUI:

```bash
# Download the CLI binary from Releases, or build from source:
cargo run -- server --port 3030 --db-path ./syncline.db
```

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

Syncing starts immediately. Install the plugin on your other devices and point them to the same server.

---

## FAQ

**Do I need the server running 24/7?**
No. Changes are stored durably in SQLite. If the server's down while you edit, your changes queue locally and sync the next time it's reachable.

**What if two devices edit the same note offline?**
Both sets of edits are merged automatically via CRDTs. You won't lose anything you typed.

**What about images and attachments?**
Fully supported. Binary files sync using content-addressed storage. If two devices modify the same binary file concurrently, both versions are kept with distinct names.

**Is my data encrypted?**
Data travels over WebSockets. For encryption in transit, put the server behind a reverse proxy with TLS (Nginx + Let's Encrypt works well). End-to-end encryption is planned but not yet implemented.

**How do I back up?**
Copy the `syncline.db` file. That single file has the full history of your vault.

**Mobile support?**
Yes. The plugin works on both desktop and mobile Obsidian.

---

## Repository

- **Main repository:** [github.com/tomas789/syncline](https://github.com/tomas789/syncline)

---

## License

MIT
