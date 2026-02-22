# 🔐 A Note on Encryption

Right now, Syncline protects your data in transit by relying on Secure WebSockets (`wss://`). This means anyone snooping on your network traffic just sees encrypted gibberish. That’s standard internet security and should be your absolute minimum for any self-hosted service exposed to the world.

**However**, as of now, data is _not_ End-to-End Encrypted (E2EE) at rest on the server. The server stores the raw CRDT updates in the SQLite database. That means, mathematically, if your server gets compromised or a hostile actor gains root access to your machine, the attacker can piece together your notes and read everything.

If you are running this on a VPS you don't fully control, or if you are storing your deepest dark secrets and banking passwords in plain text Markdown... keep this reality in mind! We are huge proponents of privacy, and it is the main reason this tool is self-hosted.

(Check out our [Future Work](future-work.md) for plans on adding proper E2EE.)
