# Syncline Obsidian Plugin

A CRDT-based synchronization plugin for Obsidian that enables real-time collaboration and offline editing with automatic conflict resolution.

## Features

- Real-time synchronization across multiple devices
- Offline editing with automatic merge when reconnecting
- Conflict-free resolution using CRDTs (Yjs)
- Cross-platform support: macOS, Windows, Linux, Android, iOS
- Simple configuration - just enter your server URL

## Installation

### Manual Installation

1. Download the latest release or build from source
2. Copy the following files to your Obsidian vault's `.obsidian/plugins/syncline-obsidian/` folder:
   - `main.js`
   - `manifest.json`
   - `styles.css`
   - `wasm/` folder (containing `syncline.js`, `syncline_bg.wasm`, and `.d.ts` files)
3. Restart Obsidian
4. Enable the plugin in Settings > Community plugins

### Building from Source

```bash
# Clone the repository
git clone https://github.com/your-repo/syncline.git
cd syncline

# Build the WASM module
cd syncline
wasm-pack build --target web --out-dir ../obsidian-plugin/wasm

# Build the plugin
cd ../obsidian-plugin
npm install
npm run build

# The plugin files are now ready in the obsidian-plugin directory
```

## Configuration

1. Open Obsidian Settings
2. Go to Community plugins > Syncline Sync
3. Enter your Syncline server URL (e.g., `ws://192.168.1.100:3030`)
4. Toggle "Auto Sync" if you want automatic synchronization on startup
5. Click "Reconnect" to connect to the server

## Testing on Different Platforms

### Prerequisites

1. Build and run the Syncline server:
   ```bash
   cd syncline
   cargo run --bin server -- --port 3030
   ```

2. Note your server's IP address:
   - On macOS/Linux: `ifconfig | grep "inet " | grep -v 127.0.0.1`
   - On Windows: `ipconfig`

### Testing on macOS / Windows / Linux (Desktop)

1. **Install the plugin** following the Manual Installation steps above

2. **Configure the server URL**:
   - Open Obsidian Settings > Syncline Sync
   - Enter `ws://SERVER_IP:3030` where SERVER_IP is your server's IP address
   - If running locally, use `ws://localhost:3030`

3. **Test synchronization**:
   - Create or edit a markdown file
   - The status bar should show "Syncline: Syncing..." then "Syncline: Synced"
   - Open the same vault on another device or use the CLI client to verify sync

4. **Test offline editing**:
   - Disconnect from the network
   - Make edits to a file
   - Reconnect and verify changes are synchronized

### Testing on Android

1. **Prerequisites**:
   - Syncline server running on your computer
   - Your Android device and computer on the same WiFi network
   - Obsidian app installed from Google Play Store

2. **Install the plugin**:
   - Connect your Android device to your computer via USB
   - Enable "File Transfer" mode on Android
   - Navigate to your Obsidian vault folder on the device
   - Create the folder `.obsidian/plugins/syncline-obsidian/`
   - Copy all plugin files (`main.js`, `manifest.json`, `styles.css`, `wasm/` folder)
   - Disconnect the device

3. **Configure and test**:
   - Open Obsidian on your Android device
   - Go to Settings > Community plugins and enable "Syncline Sync"
   - Go to Syncline Sync settings
   - Enter server URL: `ws://YOUR_COMPUTER_IP:3030`
   - Click "Reconnect"
   - The status bar should show "Syncline: Synced"

4. **Troubleshooting**:
   - Make sure your computer's firewall allows incoming connections on port 3030
   - Verify both devices are on the same WiFi network
   - Try pinging your computer from Android to verify connectivity

### Testing on iOS

1. **Prerequisites**:
   - Syncline server running on your computer
   - Your iOS device and computer on the same WiFi network
   - Obsidian app installed from App Store

2. **Install the plugin using iCloud Drive**:
   - On your computer, locate your Obsidian vault in iCloud Drive
   - Navigate to `.obsidian/plugins/`
   - Create `syncline-obsidian/` folder
   - Copy all plugin files (`main.js`, `manifest.json`, `styles.css`, `wasm/` folder)
   - Wait for iCloud to sync

3. **Alternative: Using Obsidian Git or other sync methods**:
   - Sync your vault including the `.obsidian/plugins/` folder to your iOS device

4. **Configure and test**:
   - Open Obsidian on your iOS device
   - Go to Settings > Community plugins and enable "Syncline Sync"
   - Go to Syncline Sync settings
   - Enter server URL: `ws://YOUR_COMPUTER_IP:3030`
   - Click "Reconnect"
   - The status bar should show "Syncline: Synced"

5. **Troubleshooting**:
   - iOS may block insecure (ws://) connections. For production, use `wss://` with a proper SSL certificate
   - For local testing, you may need to use a reverse proxy with SSL

## Sync Status Indicator

The status bar shows the current synchronization state:

- 🟢 **Synced**: All files are synchronized
- 🟡 **Syncing**: Synchronization in progress
- 🔴 **Error**: Connection or synchronization error
- ⚪ **Disconnected**: Not connected to server

Click the status bar to see details or reconnect.

## Using with CLI Client

You can also sync files outside of Obsidian using the CLI client:

```bash
# In the syncline directory
cargo run --bin client_folder -- --url ws://SERVER_IP:3030 --dir /path/to/vault
```

This is useful for:
- Syncing to a Linux server or NAS
- Running automated backups
- Testing synchronization without Obsidian

## Architecture

The plugin uses:
- **WASM module** (Rust/yr) for CRDT-based conflict resolution
- **WebSocket** for real-time communication with the server
- **Obsidian Vault API** for file system operations

## Limitations (POC)

This is a proof-of-concept implementation with some limitations:

1. Only `.md` and `.txt` files are synchronized
2. Binary files (images, PDFs) are not yet supported
3. File deletions are not propagated
4. No encryption - use only on trusted networks
5. Initial sync may be slow for large vaults

## Troubleshooting

### Plugin not loading

1. Check that all files are in the correct location
2. Verify `manifest.json` is valid JSON
3. Check Obsidian's developer console (View > Toggle Developer Tools) for errors

### Cannot connect to server

1. Verify server is running: `curl http://SERVER_IP:3030`
2. Check firewall settings
3. Verify URL format: `ws://IP:PORT` (not `http://`)

### WASM loading fails

1. Ensure `wasm/` folder is copied with all files
2. Check browser console for CORS or MIME type errors
3. Try a full restart of Obsidian

### Mobile-specific issues

1. **Android**: Check USB file transfer completed
2. **iOS**: Ensure iCloud sync completed
3. Both: Verify WiFi connectivity and server IP

## Development

```bash
# Watch for changes and rebuild
npm run dev

# Build WASM only
npm run build:wasm

# Build plugin only
npm run build
```

## License

MIT