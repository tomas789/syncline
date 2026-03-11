# How It Works Under the Hood

Syncline uses **CRDTs (Conflict-free Replicated Data Types)** for text files and a separate content-addressed blob system for binary files. The CRDT implementation comes from `yrs`, the Rust port of the Yjs library.

## The Problem with File-Level Sync

Most sync tools — Dropbox, Nextcloud, iCloud — work on whole files. If you change line 1 on your phone and I change line 10 on my laptop, the system sees two different files and gives up. "Conflict! Choose which one to keep!" It has no idea what either of us actually did.

This is annoying even when you're online. When you're offline for a few days, it's a disaster.

## How CRDTs Fix This

Instead of syncing entire files, Syncline syncs individual edit operations.

Every keystroke — inserting "A" at position 5, deleting "B" at position 12 — gets a globally unique ID based on the client that created it and a local clock. These IDs define a partial order over all edits across all devices, and the CRDT merge function is designed so that applying the same set of edits in any order produces the same final document. Always.

When a client connects to the server, the exchange looks like this:

1. **State vectors.** Both sides compare what they know. The client says "I have updates 1–47 from device A and 1–23 from device B." The server checks what it has.
2. **Missing updates.** They trade only the edits each side is missing. No full file transfer.
3. **Deterministic merge.** Both the server and the client fold the new edits into their local document. The math guarantees identical results regardless of arrival order.

No merge conflicts. No `_Conflict_Copy` files.

## Real-Time Sync via WebSockets

When you're online, this all happens within milliseconds. You type a letter on your phone, a tiny binary update goes to the server over a WebSocket connection, and the server broadcasts it to every other connected client. It shows up in Obsidian on your laptop almost instantly.

## Binary File Synchronization

Text files get character-level merging. Binary files — images, PDFs, attachments — obviously can't be merged that way. Syncline handles them differently:

**Content-addressable storage.** Each binary file is identified by its SHA-256 hash. The server stores blobs in a `blobs` table keyed by hash, so duplicate content is only stored once.

**Metadata CRDTs.** Binary files still have a CRDT document, but it only tracks metadata (`meta.path`, `meta.type`, `meta.blob_hash`). The actual binary content is transferred via separate `MSG_BLOB_UPDATE` messages, not through the CRDT.

**Change detection.** When a binary file changes on disk, the client computes its new SHA-256 hash and compares it against the hash stored in the CRDT. Different hash means new content, which gets uploaded to the server and pushed to all other clients.

**Conflict resolution.** Binary files use last-write-wins (LWW) based on the CRDT timestamp of the `blob_hash` field. The most recent writer's content wins. Simpler than text merging, but appropriate — you can't character-merge a JPEG.
