# 🔭 Future Work

Syncline is undeniably awesome right now, but we've got big plans to make it even more incredible. Here's a sneak peek at what we're cooking up for our future roadmap:

- **End-to-End Encryption**: Serious privacy needs serious cryptography. We want to implement true E2EE so the server just routes encrypted blob updates without ever knowing what your 3 AM shower thoughts actually say. Only your end clients hold the keys!
- **Database Compaction**: Right now, the updates table in our SQLite instance grows indefinitely. That's because it natively stores every single edit as a CRDT update! The pro? You never randomly lose any history or past state. The con? The database definitely gets _huge_ over time. We need to build a system to optionally compact the database down to the current state of the documents.
- **Built-in `wss` Support**: Nginx is great to slap in front, but inserting some certs right into the server binary and having it serve Secure WebSockets natively would be absolutely huge for ease of use.
- **Managed Servers**: For folks who don't want to run their own infrastructure or poke holes in their firewalls, we're looking into providing official, hosted, zero-knowledge Syncline servers so you can just plug your URL into the app and go.
- **Binary File Support**: Currently Syncline only synchronizes text-based files (`.md`, `.txt`). We'd love to handle images, PDFs, and other binary attachments that live in Obsidian vaults — likely via a separate binary blob channel rather than CRDTs.
- **Version History UI**: The server already stores every CRDT update, which means full edit history is preserved. Exposing this as a user-facing "time travel" feature — letting you browse and restore previous versions of any note — would be incredibly powerful.
