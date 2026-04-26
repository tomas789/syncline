import { spawn, execSync, ChildProcess } from 'child_process';
import { join } from 'path';
import * as fs from 'fs';
import { expect, browser } from '@wdio/globals';

describe('Syncline v1 E2E', () => {
    let serverProc: ChildProcess;
    let cliProc: ChildProcess;
    const port = 3040;
    const serverUrl = `ws://localhost:${port}/sync`;
    const dbPath = join(__dirname, '../../test-syncline-e2e.db');
    const folderPath = join(__dirname, '../../test-sync-folder');
    const repoRoot = join(__dirname, '../../../');

    // Poll `fn` until it returns a truthy value, or time out.
    async function waitFor<T>(
        label: string,
        fn: () => Promise<T | null | undefined | false | ''>,
        timeoutMs = 200000,
        stepMs = 500
    ): Promise<T> {
        const deadline = Date.now() + timeoutMs;
        let lastErr: unknown;
        while (Date.now() < deadline) {
            try {
                const v = await fn();
                if (v) return v as T;
            } catch (e) {
                lastErr = e;
            }
            await browser.pause(stepMs);
        }
        throw new Error(
            `waitFor("${label}") timed out after ${timeoutMs}ms${lastErr ? `: ${String(lastErr)}` : ''}`
        );
    }

    async function vaultReadText(path: string): Promise<string | null> {
        return browser.executeObsidian(async ({ app }, p) => {
            const f = (app as any).vault.getAbstractFileByPath(p);
            if (!f || !('stat' in f)) return null;
            return await (app as any).vault.read(f);
        }, path);
    }

    async function vaultReadBinaryHex(path: string): Promise<string | null> {
        return browser.executeObsidian(async ({ app }, p) => {
            const f = (app as any).vault.getAbstractFileByPath(p);
            if (!f || !('stat' in f)) return null;
            const data: ArrayBuffer = await (app as any).vault.readBinary(f);
            const u8 = new Uint8Array(data);
            let hex = '';
            for (let i = 0; i < u8.length; i++) {
                hex += u8[i].toString(16).padStart(2, '0');
            }
            return hex;
        }, path);
    }

    async function vaultHas(path: string): Promise<boolean> {
        return browser.executeObsidian(async ({ app }, p) => {
            return (app as any).vault.getAbstractFileByPath(p) !== null;
        }, path);
    }

    before(async function () {
        // Cold cargo builds can take a while.
        this.timeout(180000);

        console.log('Setting up E2E environment...');

        // Build the syncline binary synchronously. Avoids the file-lock race
        // that fires when server and CLI clients try to compile in parallel.
        console.log('Building Syncline binary...');
        execSync('cargo build --bin syncline', {
            cwd: repoRoot,
            stdio: 'inherit'
        });

        // Clean any leftover state from a previous run.
        if (fs.existsSync(dbPath)) fs.unlinkSync(dbPath);
        if (fs.existsSync(folderPath)) fs.rmSync(folderPath, { recursive: true, force: true });
        fs.mkdirSync(folderPath, { recursive: true });

        // Start server. Buffer stderr so we can wait for the listen line.
        let serverStderr = '';
        serverProc = spawn(
            'cargo',
            ['run', '--bin', 'syncline', '--', 'server', '--port', String(port), '--db-path', dbPath],
            { cwd: repoRoot, stdio: 'pipe' }
        );
        serverProc.stdout?.on('data', (d) => {
            serverStderr += String(d);
            console.log('SERVER: ' + d);
        });
        serverProc.stderr?.on('data', (d) => {
            serverStderr += String(d);
            console.error('SERVER ERR: ' + d);
        });

        // Wait for the server to actually be listening before starting the CLI.
        // `cargo run` on a cold cache can take 20–40s just to relink.
        await waitFor('server listening', async () => /listening/i.test(serverStderr), 60000);

        // Start the native CLI client (v1 via client_v1::run_client).
        let cliStderr = '';
        cliProc = spawn(
            'cargo',
            ['run', '--bin', 'syncline', '--', 'sync', '-f', folderPath, '-u', serverUrl],
            { cwd: repoRoot, stdio: 'pipe' }
        );
        cliProc.stdout?.on('data', (d) => {
            cliStderr += String(d);
            console.log('CLI: ' + d);
        });
        cliProc.stderr?.on('data', (d) => {
            cliStderr += String(d);
            console.error('CLI ERR: ' + d);
        });

        // Wait for the CLI to complete its WS handshake with the server.
        // Without this we race the first-test writes before sync is live.
        await waitFor('CLI handshake', async () => /v1 handshake OK/.test(cliStderr), 60000);

        const obsidianPage = browser.getObsidianPage();
        try {
            await obsidianPage.enablePlugin('syncline-obsidian');
            console.log('Syncline plugin enabled');
        } catch (e: any) {
            console.error('Could not enable plugin or already enabled:', e?.message);
        }

        await browser.executeObsidian(async ({ app }, url) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            if (!plugin) throw new Error('Syncline plugin not found in app.plugins');
            plugin.settings.serverUrl = url;
            await plugin.saveSettings();
            plugin.disconnect();
            await plugin.connect();
        }, serverUrl);

        // Wait for the plugin's WS client to report isConnected=true.
        await waitFor('plugin connected', async () => {
            return browser.executeObsidian(async ({ app }) => {
                const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
                return !!(plugin && plugin.client && plugin.client.isConnected());
            });
        }, 30000);
    });

    after(() => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        if (serverProc && !serverProc.killed) serverProc.kill();
    });

    // ---- 1: Obsidian → CLI create (original test preserved) ----
    it('syncs a new file Obsidian → CLI', async () => {
        const obsidianPage = browser.getObsidianPage();
        await obsidianPage.write('test.md', '# Hello from E2E');

        const target = join(folderPath, 'test.md');
        const content = await waitFor<string>('test.md on CLI side', async () => {
            if (!fs.existsSync(target)) return null;
            const c = fs.readFileSync(target, 'utf8');
            return c.includes('# Hello from E2E') ? c : null;
        });
        expect(content).toContain('# Hello from E2E');
    });

    // ---- 2: CLI → Obsidian create ----
    it('syncs a new file CLI → Obsidian', async () => {
        fs.writeFileSync(join(folderPath, 'from-cli.md'), '# Dropped from CLI');

        const content = await waitFor<string>('from-cli.md in vault', async () => {
            const c = await vaultReadText('from-cli.md');
            return c && c.includes('# Dropped from CLI') ? c : null;
        });
        expect(content).toContain('# Dropped from CLI');
    });

    // ---- 3a: Modify Obsidian → CLI ----
    it('propagates modifications Obsidian → CLI', async () => {
        const obsidianPage = browser.getObsidianPage();
        await obsidianPage.write('test.md', '# Modified from Obsidian');

        const target = join(folderPath, 'test.md');
        const content = await waitFor<string>('test.md modified on CLI side', async () => {
            if (!fs.existsSync(target)) return null;
            const c = fs.readFileSync(target, 'utf8');
            return c.includes('# Modified from Obsidian') ? c : null;
        });
        expect(content).toContain('# Modified from Obsidian');
    });

    // ---- 3b: Modify CLI → Obsidian ----
    it('propagates modifications CLI → Obsidian', async () => {
        fs.writeFileSync(join(folderPath, 'test.md'), '# Modified from CLI');

        const content = await waitFor<string>('test.md modified in vault', async () => {
            const c = await vaultReadText('test.md');
            return c && c.includes('# Modified from CLI') ? c : null;
        });
        expect(content).toContain('# Modified from CLI');
    });

    // ---- 4: Rename Obsidian → CLI ----
    it('propagates renames Obsidian → CLI', async () => {
        await browser.executeObsidian(async ({ app }) => {
            const f = (app as any).vault.getAbstractFileByPath('test.md');
            if (!f) throw new Error('test.md not found in vault before rename');
            await (app as any).fileManager.renameFile(f, 'renamed.md');
        });

        await waitFor('rename on CLI side', async () => {
            const oldGone = !fs.existsSync(join(folderPath, 'test.md'));
            const newHere = fs.existsSync(join(folderPath, 'renamed.md'));
            return oldGone && newHere;
        });
    });

    // ---- 5: Delete CLI → Obsidian ----
    it('propagates deletes CLI → Obsidian', async () => {
        const t0 = new Date().toISOString();
        const target = join(folderPath, 'renamed.md');
        const existsBefore = fs.existsSync(target);
        console.log(`[FLAKE-REPRO] ${t0} test: about to unlinkSync ${target} (exists=${existsBefore})`);
        fs.unlinkSync(target);
        const tUnlink = new Date().toISOString();
        console.log(`[FLAKE-REPRO] ${tUnlink} test: unlinkSync done`);

        let pollCount = 0;
        let lastSeen = true;
        await waitFor('renamed.md removed from vault', async () => {
            pollCount++;
            const seen = await vaultHas('renamed.md');
            // Log every poll for the first few, then on transitions only.
            if (pollCount <= 3 || pollCount % 50 === 0 || seen !== lastSeen) {
                console.log(`[FLAKE-REPRO] ${new Date().toISOString()} test: poll ${pollCount} vault.has(renamed.md)=${seen}`);
                lastSeen = seen;
            }
            return !seen;
        });
        console.log(`[FLAKE-REPRO] ${new Date().toISOString()} test: convergence reached after ${pollCount} polls`);
    });

    // ---- 6: Binary blob roundtrip CLI → Obsidian ----
    it('roundtrips a binary blob CLI → Obsidian', async () => {
        // PNG magic header + non-textual payload (includes 0x00). Not a valid
        // PNG — Syncline doesn't care, it's just bytes through the blob path.
        const bytes = Buffer.from([
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
            0x00, 0x01, 0x02, 0x03, 0xfe, 0xfd, 0xfc, 0xfb,
            0xde, 0xad, 0xbe, 0xef,
        ]);
        const expectedHex = bytes.toString('hex');

        fs.writeFileSync(join(folderPath, 'pixel.png'), bytes);

        const gotHex = await waitFor<string>('pixel.png bytes in vault', async () => {
            const h = await vaultReadBinaryHex('pixel.png');
            return h === expectedHex ? h : null;
        }, 30000);
        expect(gotHex).toBe(expectedHex);
    });

    // ---- 7: `syncline verify` after sync settles ----
    it('passes `syncline verify` after sync settles', async function () {
        this.timeout(60000);

        // Let any pending sync drain.
        await browser.pause(3000);

        // Stop the background `sync` process so `verify` can read the
        // local `.syncline/` state without contention.
        if (cliProc && !cliProc.killed) {
            cliProc.kill();
            await new Promise<void>((resolve) => {
                const done = () => resolve();
                cliProc.once('exit', done);
                setTimeout(done, 3000);
            });
        }

        // `syncline verify` exits 0 iff the local projection hash matches
        // the server. execSync throws on non-zero exit.
        const result = execSync(
            `cargo run --bin syncline -- verify -f "${folderPath}" -u ${serverUrl} -t 8`,
            { cwd: repoRoot, encoding: 'utf8' }
        );
        console.log('VERIFY:', result);
        expect(result).toBeDefined();
    });
});
