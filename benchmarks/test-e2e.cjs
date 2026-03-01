const fs = require('fs');
const path = require('path');
const { spawn } = require('child_process');

(async () => {
    console.log('Building E2E Setup...');
    
    const vaultPath = path.join(__dirname, 'test-vault-e2e');
    const folderPath = path.join(__dirname, 'test-folder-e2e');
    const dbPath = path.join(__dirname, 'test-syncline-e2e.db');

    // Clean up
    if (fs.existsSync(vaultPath)) fs.rmSync(vaultPath, { recursive: true, force: true });
    if (fs.existsSync(folderPath)) fs.rmSync(folderPath, { recursive: true, force: true });
    if (fs.existsSync(dbPath)) fs.rmSync(dbPath);

    fs.mkdirSync(vaultPath);
    fs.mkdirSync(folderPath);

    // Setup obsidian vault
    const obsidianDir = path.join(vaultPath, '.obsidian');
    fs.mkdirSync(obsidianDir);
    
    const pluginsDir = path.join(obsidianDir, 'plugins');
    fs.mkdirSync(pluginsDir);
    
    // Inject syncline
    const synclinePluginDir = path.join(pluginsDir, 'syncline');
    fs.mkdirSync(synclinePluginDir);
    
    // Copy the dist files
    const srcDir = path.join(__dirname, '../obsidian-plugin');
    fs.cpSync(path.join(srcDir, 'main.js'), path.join(synclinePluginDir, 'main.js'));
    fs.cpSync(path.join(srcDir, 'manifest.json'), path.join(synclinePluginDir, 'manifest.json'));
    
    // Enable the plugin
    fs.writeFileSync(path.join(obsidianDir, 'community-plugins.json'), JSON.stringify(['syncline']));
    
    // Prepare plugin data
    fs.writeFileSync(path.join(synclinePluginDir, 'data.json'), JSON.stringify({
        url: "ws://localhost:3039/sync"
    }));
    
    console.log('Vault configured.');
    
    // Start Server
    console.log('Starting Syncline Server...');
    const serverProc = spawn('cargo', ['run', '--', 'server', '--port', '3039', '--db-path', dbPath], {
        cwd: path.join(__dirname, '..'),
        stdio: 'ignore'
    });

    await new Promise(r => setTimeout(r, 2000));

    // Start CLI
    console.log('Starting Syncline CLI...');
    const cliProc = spawn('cargo', ['run', '--', 'sync', '-f', folderPath, '-u', 'ws://localhost:3039/sync'], {
        cwd: path.join(__dirname, '..'),
        stdio: 'ignore'
    });

    await new Promise(r => setTimeout(r, 2000));

    // Start Obsidian
    console.log('Starting Obsidian...');
    const obsidianProc = spawn('/Applications/Obsidian.app/Contents/MacOS/Obsidian', [vaultPath], {
        stdio: 'ignore'
    });

    // Wait for obsidian to sync or connect
    await new Promise(r => setTimeout(r, 5000));

    // Now test a simple synchronization: write to vault
    console.log('Writing test file in Vault...');
    fs.writeFileSync(path.join(vaultPath, 'hello.md'), 'Hello from Vault');

    await new Promise(r => setTimeout(r, 5000));

    // Check if it appeared in the folder
    const targetFile = path.join(folderPath, 'hello.md');
    if (fs.existsSync(targetFile)) {
        const content = fs.readFileSync(targetFile, 'utf8');
        console.log('SUCCESS: Content in synced folder:', content);
    } else {
        console.error('FAILED: File was not synced to the CLI folder.');
    }

    obsidianProc.kill();
    cliProc.kill();
    serverProc.kill();
})();
