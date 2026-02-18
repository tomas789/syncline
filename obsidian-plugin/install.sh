#!/bin/bash

set -e

VAULT_PATH="$1"

if [ -z "$VAULT_PATH" ]; then
    echo "Usage: $0 <path-to-obsidian-vault>"
    echo "Example: $0 ~/Documents/MyObsidianVault"
    exit 1
fi

if [ ! -d "$VAULT_PATH" ]; then
    echo "Error: Vault directory does not exist: $VAULT_PATH"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_DIR="$VAULT_PATH/.obsidian/plugins/syncline-obsidian"

echo "Installing Syncline Obsidian Plugin..."
echo "  Vault: $VAULT_PATH"
echo "  Plugin: $PLUGIN_DIR"

if [ ! -f "$SCRIPT_DIR/main.js" ]; then
    echo ""
    echo "Plugin not built. Building now..."
    cd "$SCRIPT_DIR"
    npm install
    npm run build
fi

if [ ! -d "$SCRIPT_DIR/wasm" ] || [ ! -f "$SCRIPT_DIR/wasm/syncline_bg.wasm" ]; then
    echo ""
    echo "WASM not built. Building now..."
    cd "$SCRIPT_DIR/../syncline"
    wasm-pack build --target web --out-dir ../obsidian-plugin/wasm
fi

mkdir -p "$PLUGIN_DIR"
mkdir -p "$PLUGIN_DIR/wasm"

cp "$SCRIPT_DIR/main.js" "$PLUGIN_DIR/"
cp "$SCRIPT_DIR/manifest.json" "$PLUGIN_DIR/"
cp "$SCRIPT_DIR/styles.css" "$PLUGIN_DIR/"
cp -r "$SCRIPT_DIR/wasm/"* "$PLUGIN_DIR/wasm/"

echo ""
echo "Installation complete!"
echo ""
echo "Next steps:"
echo "1. Open Obsidian"
echo "2. Go to Settings > Community plugins"
echo "3. Enable 'Syncline Sync'"
echo "4. Configure the server URL in Syncline Sync settings"
