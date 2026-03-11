# A Note on Encryption

Syncline protects data in transit through Secure WebSockets (`wss://`). Traffic between clients and the server is encrypted, which is the baseline you should expect from anything exposed to the internet.

**Data at rest is not encrypted.** The server stores raw CRDT updates in the SQLite database. If someone gets root access to your server, they can reconstruct your notes and read everything.

If you're running this on a VPS you don't fully control, keep that in mind. The whole point of self-hosting is that *you* control the machine. If you can't trust the machine, the privacy guarantees don't hold.

End-to-end encryption is on the roadmap. See the [future work](future-work.md) page for more on that.
