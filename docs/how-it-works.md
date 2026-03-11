# 🧠 How It Works Under the Hood

Syncline isn't just a file copier—it has a deep understanding of text changes. It does this through **CRDTs (Conflict-free Replicated Data Types)**, specifically by using the fantastic `yrs` (Yjs for Rust) library.

## The Problem with Sync

Usually, syncing systems like Dropbox or Nextcloud work on the _file level_. If I change `line 1` on my phone, and you change `line 10` on your PC, and we both sync... the system throws its hands up. "Conflict! Choose which file to keep!" This happens because it just sees two different files, and it doesn't know what you actually meant.

## The CRDT Solution

Instead of syncing the whole file, Syncline syncs _intent_.
Every time you take an action—like pressing "A" or deleting "B" at index 5—that action is given a locally unique ID.
When the server and client connect, they don't say "Here's my version of the file." They say, "Here's a mathematical vector of exactly which keystrokes I know about."

1. **State Vectors**: The server and client compare notes, realizing that the server knows about your friend's edits, and the client knows about your offline edits.
2. **Missing Updates**: They trade only the exact modifications each is missing.
3. **Deterministic Merge**: Due to the math of CRDTs, both the server and the client can take those independent edits, toss them into a blender, and end up with the _exact same document_ every time, guaranteed.

No merge conflicts, no weird duplicated `_Conflict_Copy` files, just seamless syncing magic.

## Real-Time & WebSockets

When you're online, all of this happens basically instantly over WebSockets. The second you tap a letter on your phone, an incredibly tiny binary update is shot to the server, and the server broadcasts that letter out to your desktop where it magically pops up right within Obsidian.

## Binary File Synchronization

Not everything is text. Images, PDFs, and attachments are **binary files** that can't be merged character-by-character like text. Syncline handles these with a different strategy:

1. **Content-Addressable Storage**: Each binary file is identified by its **SHA-256 hash**. The server stores blobs in a content-addressable `blobs` table — if two files have the same content, they share the same blob.

2. **Metadata CRDTs**: Binary files still use a CRDT document, but only for **metadata** (`meta.path`, `meta.type`, `meta.blob_hash`). The actual binary content is transferred via separate `MSG_BLOB_UPDATE` messages, not through the CRDT.

3. **Change Detection**: When a binary file is modified, the client computes its new SHA-256 hash and compares it with the hash stored in the CRDT. If they differ, the new blob is uploaded to the server and broadcast to all other clients.

4. **Conflict Resolution**: Binary files use **Last-Write-Wins (LWW)** based on the CRDT timestamp of the `blob_hash` field. The latest writer's content wins. This is simpler than text merging but appropriate for binary data where character-level merging isn't possible.
