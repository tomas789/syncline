# 💾 Dealing with Backups

Because Syncline uses an append-only, CRDT-based database structure, you have an inherent history of updates. But servers break, hard drives fry, and databases get corrupted. Even cool tools like this one need a solid backup strategy.

## How to back up your data

The easiest and safest way to back up your Syncline setup is to backup the server's SQLite database file (`syncline.db`). This file contains the entirety of your workspace sync history and the current state.

### Live Server Backup

Because SQLite is phenomenal, you can actually back it up while the server is actively running without taking it offline. You can use the standard SQLite backup API or just use the CLI:

```bash
sqlite3 syncline.db ".backup 'syncline_backup.db'"
```

Set that up on a cron job, ship the backup to an S3 bucket or another drive, and you are literally golden. 🏆

### Folder Level Backup

Another awesome way to back up? Just run an instance of `client_folder` on a separate, secure machine (like a NAS). Let it sync the vault automatically in the background.

```bash
syncline sync -f /path/to/my/backup/folder -u ws://my-server:3030/sync
```

Whenever anyone edits anything in Obsidian, `client_folder` quietly pulls down the update and syncs it straight into plain text Markdown right there. Presto, you have a constantly updated plain-text, human-readable backup of all your Markdown files!
