// Regression test for #57 — STEP_1 dropped before WS handshake.
//
// Scenario:
//   1. Server is populated with N text files (content lives on the server).
//   2. Plugin connects fresh, fully converges, content lands on Obsidian disk.
//   3. Disconnect the plugin (does NOT wipe the persisted manifest.bin).
//   4. WIPE Obsidian's vault files AND the plugin's `v1/content/` cache —
//      so that on reconnect the plugin must re-subscribe and re-fetch
//      every content subdoc.
//   5. plugin.connect() again. With a persisted manifest, the plugin's
//      reconcile loop runs immediately on connect() — *before* the WS
//      handshake completes. In the buggy code, every `subscribeContent`
//      sent during this gap silently no-ops because `is_connected` is
//      false; the WASM client doesn't queue them, the TS wrapper still
//      adds the nodeId to `subscribedContent`, and reconnects don't
//      re-fire because the set says "already subscribed".
//   6. Assert every text file's content is restored on Obsidian disk.
//
// With the bug present: a chunk of files stays as 0-byte placeholders
// forever. With the fix: every file has its full content.

import { spawn, ChildProcess } from 'child_process';
import { join, relative } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline #57 — STEP_1 dropped before WS handshake', () => {
    const port = 3057;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue57.db');
    const cliFolderPath = join(e2eDir, 'issue57-cli-vault');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    const N_FILES = 200;
    const EMPTY_SHA = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';

    let serverProc: ChildProcess;
    let cliProc: ChildProcess;

    function fileSha(p: string): string {
        return crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
    }
    function listVault(root: string): Map<string, string> {
        const out = new Map<string, string>();
        function walk(dir: string) {
            for (const ent of fs.readdirSync(dir, { withFileTypes: true })) {
                if (ent.name.startsWith('.')) continue;
                if (ent.name.endsWith('.tmp')) continue;
                const abs = join(dir, ent.name);
                if (ent.isDirectory()) walk(abs);
                else if (ent.isFile()) out.set(relative(root, abs), fileSha(abs));
            }
        }
        if (fs.existsSync(root)) walk(root);
        return out;
    }
    async function waitFor<T>(label: string, fn: () => Promise<T | null | undefined | false | ''>, timeoutMs = 60_000, stepMs = 200): Promise<T> {
        const deadline = Date.now() + timeoutMs;
        while (Date.now() < deadline) {
            try { const v = await fn(); if (v) return v as T; } catch {}
            await new Promise((r) => setTimeout(r, stepMs));
        }
        throw new Error(`waitFor("${label}") timed out after ${timeoutMs}ms`);
    }

    function makeCorpus(): Map<string, string> {
        // Mix of paths under several subdirs so reconcile has nontrivial
        // ensureParentFolders work that widens the race window.
        const files = new Map<string, string>();
        for (let i = 0; i < N_FILES; i++) {
            const sizeBytes = 100 + ((i * 211) % 4_000);
            const seed = crypto.createHash('sha256').update(`#57-file-${i}`).digest('hex');
            let content = `# file ${i}\n\n`;
            while (content.length < sizeBytes) content += seed + '\n';
            const subdir = `dir-${(i % 10).toString().padStart(2, '0')}`;
            files.set(`${subdir}/note-${String(i).padStart(4, '0')}.md`, content);
        }
        return files;
    }

    before(async function () {
        this.timeout(2 * 60_000);
        if (!fs.existsSync(synclineBin)) throw new Error(`syncline binary missing at ${synclineBin}`);
        if (fs.existsSync(dbPath)) fs.unlinkSync(dbPath);
        if (fs.existsSync(cliFolderPath)) fs.rmSync(cliFolderPath, { recursive: true, force: true });
        fs.mkdirSync(cliFolderPath, { recursive: true });

        let serverOut = '';
        serverProc = spawn(synclineBin, ['server', '--port', String(port), '--db-path', dbPath], { stdio: 'pipe' });
        serverProc.stdout?.on('data', (d) => { serverOut += d; });
        serverProc.stderr?.on('data', (d) => { serverOut += d; });
        await waitFor('server listening', async () => /listening/i.test(serverOut), 30_000, 100);

        let cliOut = '';
        cliProc = spawn(synclineBin, ['sync', '-f', cliFolderPath, '-u', serverUrl, '--log-level', 'info'], { stdio: 'pipe' });
        cliProc.stdout?.on('data', (d) => { cliOut += d; });
        cliProc.stderr?.on('data', (d) => { cliOut += d; });
        await waitFor('CLI handshake', async () => /v1 handshake OK/.test(cliOut), 30_000, 100);

        // Drop the corpus into the CLI dir. CLI scanner will detect and
        // upload everything to the server.
        const corpus = makeCorpus();
        for (const [name, content] of corpus) {
            const full = join(cliFolderPath, name);
            fs.mkdirSync(join(cliFolderPath, name.split('/').slice(0, -1).join('/')), { recursive: true });
            fs.writeFileSync(full, content);
        }

        const obsidianPage = browser.getObsidianPage();
        try { await obsidianPage.enablePlugin('syncline-obsidian'); } catch {}
        await browser.executeObsidian(async ({ app }, url) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            if (!plugin) throw new Error('plugin not found');
            plugin.settings.serverUrl = url;
            await plugin.saveSettings();
            plugin.disconnect();
            await plugin.connect();
        }, serverUrl);
        await waitFor('plugin connected', async () => browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            return !!(plugin && plugin.client && plugin.client.isConnected());
        }), 30_000, 200);
    });

    after(() => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        if (serverProc && !serverProc.killed) serverProc.kill();
    });

    it('content survives a plugin restart with cached manifest', async function () {
        this.timeout(5 * 60_000);

        const vaultPath: string = await browser.executeObsidian(async ({ app }) => (app as any).vault.adapter.basePath as string);
        const corpus = makeCorpus();
        const expected = new Map<string, string>();
        for (const [name, content] of corpus) {
            expected.set(name, crypto.createHash('sha256').update(content).digest('hex'));
        }

        // Phase A: wait for initial convergence on Obsidian disk.
        await waitFor('initial convergence', async () => {
            const obs = listVault(vaultPath);
            for (const [p, sha] of expected) {
                const got = obs.get(p);
                if (got !== sha) return false;
            }
            return true;
        }, 2 * 60_000, 200);
        console.log(`[#57] phase A: initial convergence done`);

        // Phase B: disconnect, then wipe vault md files + plugin v1/content/.
        // Keep manifest.bin so the next connect()'s reconcile sees the
        // full projection synchronously and races the WS handshake.
        await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            plugin.disconnect();
        });

        const stateDir = join(vaultPath, '.obsidian', 'plugins', 'syncline-obsidian', 'v1');
        if (fs.existsSync(join(stateDir, 'content'))) {
            for (const f of fs.readdirSync(join(stateDir, 'content'))) {
                fs.unlinkSync(join(stateDir, 'content', f));
            }
        }
        // Wipe vault .md / .txt files (keep dirs and dotfiles).
        function wipeFiles(dir: string) {
            for (const ent of fs.readdirSync(dir, { withFileTypes: true })) {
                if (ent.name.startsWith('.')) continue;
                const abs = join(dir, ent.name);
                if (ent.isDirectory()) wipeFiles(abs);
                else if (ent.name.endsWith('.md') || ent.name.endsWith('.txt')) fs.unlinkSync(abs);
            }
        }
        wipeFiles(vaultPath);

        // Sanity: manifest.bin still there, content/.bin gone, vault empty of .md.
        if (!fs.existsSync(join(stateDir, 'manifest.bin'))) {
            throw new Error('manifest.bin missing — wipe was too aggressive');
        }
        const remainingMds = [...listVault(vaultPath).keys()].filter((k) => k.endsWith('.md')).length;
        if (remainingMds !== 0) {
            throw new Error(`vault still has ${remainingMds} .md files — wipe failed`);
        }
        console.log(`[#57] phase B: cache + vault wiped, manifest.bin retained`);

        // Phase C: reconnect — race fires.
        await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            await plugin.connect();
        });
        await waitFor('plugin re-connected', async () => browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            return !!(plugin && plugin.client && plugin.client.isConnected());
        }), 30_000, 100);
        console.log(`[#57] phase C: reconnected`);

        // Phase D: wait for content to arrive again. Timeout means the bug
        // is present (some subset of files will never get their STEP_1
        // resent — see the issue body).
        let lastNonEmpty = -1;
        let stableTicks = 0;
        await waitFor('content reconverged', async () => {
            const obs = listVault(vaultPath);
            let ok = true;
            let nonEmpty = 0;
            for (const [p, sha] of expected) {
                const got = obs.get(p);
                if (got === sha) nonEmpty++;
                else ok = false;
            }
            if (nonEmpty === lastNonEmpty) stableTicks++; else stableTicks = 0;
            lastNonEmpty = nonEmpty;
            if (stableTicks % 10 === 0) {
                console.log(`[#57] reconverge progress: ${nonEmpty}/${expected.size} stable=${stableTicks}`);
            }
            // Either we converged OR we've been stable for 10s and never
            // will (bug fingerprint — bail with a clear failure).
            return ok || (stableTicks >= 50 && nonEmpty < expected.size);
        }, 3 * 60_000, 200);

        const obs = listVault(vaultPath);
        const missing: string[] = [];
        for (const [p, sha] of expected) {
            const got = obs.get(p);
            if (got !== sha) {
                missing.push(`${p} [got=${got ?? 'MISSING'}]`);
            }
        }
        if (missing.length > 0) {
            console.error(`[#57] ${missing.length} files NOT reconverged after restart:`);
            for (const m of missing.slice(0, 10)) console.error(`   ${m}`);
        }
        expect(missing.length).toBe(0);
    });
});
