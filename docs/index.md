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

# 🚀 Getting Started

Syncline has two main ways to sync: directly hitting the server from inside your Obsidian app, or running a background client that watches a regular folder.

## 🔮 General usage of server with Obsidian

Want to sync your mobile phone or tablet? The Obsidian plugin is what you need.

1. **Fire up the Server**:
   Grab the server binary or compile it, and run it on a machine that's always on (like a Raspberry Pi, a VPS, or your home server).
   ```bash
   cargo run -p server -- --port 3030 --db-path syncline.db
   ```
2. **Install the Plugin**:
   Install the Syncline Obsidian plugin on your devices. You can install it straight out of the community plugins list, or compile it manually.
3. **Connect**:
   In the plugin settings, enter your server's WebSocket address (e.g., `ws://your-server-ip:3030/sync`). If you are running over the public internet, definitely make sure to use `wss://`!
4. **Boom**: Watch your notes sync up instantly. It really is that easy.

---

## 🗂️ Usage of the `client_folder`

If you are on a desktop or laptop and prefer to just use a regular folder (or want a non-Obsidian headless sync client), use `client_folder`. It runs in the background, watches your files for changes, and beams them directly to the server.

1. **Run the watcher**:
   Just point it at your local directory and tell it where the server lives:
   ```bash
   cargo run -p client_folder -- -f /path/to/your/notes -u ws://localhost:3030/sync
   ```
2. **Edit away**: Use Vim, Emacs, VS Code, or whatever text editor you prefer. The client will automatically pick up your saves and sync them via the core CRDT mechanism seamlessly with the server and any other connected devices.

---

## 🧐 What is this wizardry? (Layman's Explanation)

Imagine you and your friend are both editing the same Google Doc, but neither of you has internet access. You add a new paragraph, your friend deletes a sentence. When you both get back online, instead of shouting "FILE CONFLICT! CHOOSE WHICH ONE TO KEEP!", a magical arbiter looks at exactly _what_ you pressed and weaves both your changes together perfectly.

That's what Syncline does for your Markdown files. Under the hood, it uses something called **CRDTs (Conflict-free Replicated Data Types)**.

When you type a letter, Syncline doesn't just save the whole file; it remembers that specific keystroke and where it lives. The server acts as a giant mailroom, collecting these extremely specific, tiny updates. When devices connect, they just trade the updates they missed. The math behind the CRDT guarantees that no matter what order the updates arrive in, the final document will look exactly the same on every device. It's like Git, but completely automatic and down to the individual character level!

---

Read more about [How It Works](how-it-works.md) under the hood. 🚀
