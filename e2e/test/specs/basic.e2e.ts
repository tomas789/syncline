import { spawn, execSync, ChildProcess } from 'child_process';
import { join } from 'path';
import * as fs from 'fs';
import { expect, browser } from '@wdio/globals';

describe('Syncline E2E Configuration', () => {
    let serverProc: ChildProcess;
    let cliProc: ChildProcess;
    const dbPath = join(__dirname, '../../test-syncline-e2e.db');
    const folderPath = join(__dirname, '../../test-sync-folder');

    before(async function () {
        // Allow up to 2 minutes for initial cargo build on cold starts
        this.timeout(120000); 

        console.log("Setting up E2E environment...");
        
        // Pre-build the binary synchronously to avoid file-lock errors when both the 
        // CLI client and the server simultaneously try to compile 'syncline'.
        console.log("Building Syncline binary...");
        execSync('cargo build --bin syncline', {
            cwd: join(__dirname, '../../../'),
            stdio: 'inherit'
        });

        // Clean up from previous run
        if (fs.existsSync(dbPath)) fs.unlinkSync(dbPath);
        if (fs.existsSync(folderPath)) fs.rmSync(folderPath, { recursive: true, force: true });
        fs.mkdirSync(folderPath, { recursive: true });

        // Setup the plugin data file with the dev server URL
        const dataJsonPath = join(__dirname, '../../test-vault/.obsidian/plugins/syncline/data.json');
        if (!fs.existsSync(join(__dirname, '../../test-vault/.obsidian/plugins/syncline'))) {
            fs.mkdirSync(join(__dirname, '../../test-vault/.obsidian/plugins/syncline'), { recursive: true });
        }
        fs.writeFileSync(dataJsonPath, JSON.stringify({
            url: "ws://localhost:3040/sync"
        }));

        // Start server
        serverProc = spawn('cargo', ['run', '--bin', 'syncline', '--', 'server', '--port', '3040', '--db-path', dbPath], {
            cwd: join(__dirname, '../../../'),
            stdio: 'pipe'
        });
        serverProc.stdout?.on('data', d => console.log('SERVER: ' + d));
        serverProc.stderr?.on('data', d => console.error('SERVER ERR: ' + d));

        // Wait a bit
        await browser.pause(2000);

        // Start CLI
        cliProc = spawn('cargo', ['run', '--bin', 'syncline', '--', 'sync', '-f', folderPath, '-u', 'ws://localhost:3040/sync'], {
            cwd: join(__dirname, '../../../'),
            stdio: 'pipe'
        });
        cliProc.stdout?.on('data', d => console.log('CLI: ' + d));
        cliProc.stderr?.on('data', d => console.error('CLI ERR: ' + d));

        await browser.pause(2000);
    });

    after(() => {
        if (serverProc && !serverProc.killed) serverProc.kill();
        if (cliProc && !cliProc.killed) cliProc.kill();
    });

    it('should boot up Obsidian and sync a new file to CLI', async () => {
        // Wait for Obsidian to initialize and sync
        await browser.pause(5000);

        const obsidianPage = browser.getObsidianPage();
        try {
            await obsidianPage.enablePlugin('syncline-obsidian');
            console.log("Syncline plugin enabled");
        } catch (e) {
            console.error("Could not enable plugin or already enabled:", e?.message);
        }

        await browser.executeObsidian(async ({ app }) => {
            const plugin = (app as any).plugins.plugins["syncline-obsidian"];
            if (plugin) {
                plugin.settings.serverUrl = "ws://localhost:3040/sync";
                await plugin.saveSettings();
                
                // Disconnect first just in case
                plugin.disconnect();
                await plugin.connect();
            } else {
                throw new Error("Syncline plugin not found in app.plugins");
            }
        });

        const title = await browser.getTitle();
        console.log('Obsidian window title:', title);

        // We can create a file in the vault using obsidianPage
        await obsidianPage.write('test.md', '# Hello from E2E');
        
        // Wait for propagation
        let synced = false;
        const targetFile = join(folderPath, 'test.md');
        for (let i = 0; i < 20; i++) {
            if (fs.existsSync(targetFile)) {
                synced = true;
                break;
            }
            await browser.pause(1000);
        }

        expect(synced).toBe(true);
        const content = fs.readFileSync(targetFile, 'utf8');
        expect(content).toContain('# Hello from E2E');
    });
});
