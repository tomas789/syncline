# Dealing with Backups

Syncline's append-only CRDT database gives you built-in edit history. But servers die, drives fail, databases corrupt. You still need backups.

## Backing Up the Database

The safest approach: back up the server's SQLite database file (`syncline.db`). It contains the full sync history and current state of every document.

### While the Server Is Running

SQLite handles concurrent access well enough that you can take a backup without stopping the server:

```bash
sqlite3 syncline.db ".backup 'syncline_backup.db'"
```

Throw that in a cron job, ship the backup file to S3 or another drive, and you're covered.

### Using the Folder Client as a Live Backup

You can also run a `syncline sync` instance on a separate machine (a NAS, a second VPS, whatever) and let it pull down every edit in real time:

```bash
syncline sync -f /path/to/my/backup/folder -u ws://my-server:3030/sync
```

This gives you a constantly-updated, plain-text Markdown copy of the entire vault. If the server dies, you still have readable files.
