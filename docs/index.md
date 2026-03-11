<p align="center">
  <img src="logo-with-name.jpg" alt="Syncline — Self-hosted Obsidian Sync" width="400">
</p>

# 🌊 Introduction & Usage

Hey there! Built a cool vault in Obsidian and want to keep it synced everywhere completely on your own terms? Enter **Syncline**.

Syncline is a privacy-first, self-hosted synchronization system that magically keeps your Obsidian notes in perfect harmony across all your devices, even if you've been offline for weeks taking a digital detox. No merge conflicts, no lost data, just your thoughts living rent-free across your entire ecosystem.

## ⚖️ Why Syncline?

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

## ✨ Key Benefits

- **🔒 Privacy-First**: Your notes never leave your infrastructure. The Syncline server runs on hardware you control. No telemetry, no analytics, no third-party access.
- **📁 Single-File Server Storage**: All vault data is stored in a single SQLite database file on your server. Backup is as simple as copying one file.
- **⚡ Fast & Lightweight**: Built in Rust, the server is extremely efficient. It uses WebSockets for real-time communication, meaning changes appear on your other devices within milliseconds.
- **✈️ True Offline Mode**: Work without an internet connection for hours, days, or weeks. When you reconnect, Syncline automatically merges all changes.
- **🤝 No Conflict Dialogs — Ever**: Syncline uses CRDTs (Conflict-free Replicated Data Types). When two devices edit the same note simultaneously, the changes are merged mathematically.
- **🖼️ Binary File Synchronization**: Images, PDFs, attachments — all synchronized using content-addressed storage.

---

## 🚀 Getting Started

### Step 1: Run the Syncline Server

The easiest way to run the server on your PC or Mac is to use the native **Syncline Desktop App**. It runs silently in your system tray, manages the database for you, and securely provides the connection URLs you need.

**Download the latest installer from the [Releases Page](https://github.com/tomas789/syncline/releases):**

- **Mac:** Download the `.dmg`
- **Windows:** Download the `.msi` or `.nsis.zip`

Once installed, it will automatically start with your computer. Just click the Syncline menu bar icon and select **Start Server**!

---

**Advanced/CLI Users:**
If you prefer to run the standalone headless CLI (e.g., on a headless Linux VPS or Raspberry Pi):

```bash
# Download the unified CLI binary from Releases, or build from source:
syncline server --port 3030 --db-path ./syncline.db
```

**Run the CLI Client:**
If you prefer to run the standalone headless CLI client rather than using the Obsidian plugin:

```bash
# Point the client to your vault folder and the server URL
syncline sync -f /path/to/my/vault -u ws://127.0.0.1:3030/sync
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

## 🧐 What is this wizardry? (Layman's Explanation)

Imagine you and your friend are both editing the same Google Doc, but neither of you has internet access. You add a new paragraph, your friend deletes a sentence. When you both get back online, instead of shouting "FILE CONFLICT! CHOOSE WHICH ONE TO KEEP!", a magical arbiter looks at exactly _what_ you pressed and weaves both your changes together perfectly.

That's what Syncline does for your Markdown files. Under the hood, it uses something called **CRDTs (Conflict-free Replicated Data Types)**.

When you type a letter, Syncline doesn't just save the whole file; it remembers that specific keystroke and where it lives. The server acts as a giant mailroom, collecting these extremely specific, tiny updates. When devices connect, they just trade the updates they missed. The math behind the CRDT guarantees that no matter what order the updates arrive in, the final document will look exactly the same on every device. It's like Git, but completely automatic and down to the individual character level!

---

Read more about [How It Works](how-it-works.md) under the hood. 🚀
