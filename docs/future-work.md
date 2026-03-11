# Future Work

Some things I want to build but haven't gotten to yet:

**End-to-end encryption.** The server currently sees plaintext CRDT updates. I want it to only route encrypted blobs, so even a compromised server can't read your notes. The clients would hold all the keys.

**Database compaction.** The SQLite database stores every single edit as a CRDT update, which means it grows forever. Good for history, bad for disk space on long-running vaults. There should be a way to compact down to the current document state and drop old updates.

**Native TLS.** Right now you need Nginx or similar in front of the server for `wss://`. Baking certificate handling into the server binary directly would cut out that dependency for people who don't want to mess with reverse proxy configs.

**Managed hosting.** For people who don't want to run their own server or deal with port forwarding, I'm looking into offering hosted zero-knowledge Syncline instances. Plug in a URL and go.

**Version history UI.** The server already stores the full edit history — that's how CRDTs work. Exposing that as a "time travel" feature in the plugin, so you can browse and restore older versions of any note, would be useful.
