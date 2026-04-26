// Regression test for #58 — ingestNewFile creates conflict copies of every
// existing vault file when synthetic onFileCreate events fire faster than
// the first reconcile pass populates `lastProjection`.
//
// Real-world trigger: user wipes `~/.obsidian/plugins/<id>/v1/` (e.g. as a
// workaround for #57 or after a BRAT update) and reopens Obsidian. The
// vault on disk is populated; Obsidian fires a synthetic `onFileCreate`
// for every existing file as it bootstraps the in-memory tree. The plugin's
// `ingestNewFile` runs for each, checks `lastProjection.has(file.path)` —
// false, because the first reconcile pass hasn't run yet — and falls
// through to `createText(path, size)`, which fails because the *live*
// projection (already populated by the server's manifest STEP_2) has the
// path. The catch block then calls `createTextAllowingCollision`, which
// silently mints a fresh node at the same path → projection adds a
// `.conflict-<actor>-<lamp>` suffix to one of them → the user ends up
// with N phantom conflict copies on every restart.
//
// Repro shape (deterministic, no need to actually restart Obsidian):
//   1. Populate server + plugin's vault with N text files via the CLI.
//   2. Wait for full convergence.
//   3. plugin.disconnect()
//   4. Wipe `v1/manifest.bin` (so `lastProjection` starts empty on next
//      connect). Keep the vault files on disk (Obsidian's TFile index
//      still has them).
//   5. plugin.connect()
//   6. Immediately invoke `plugin.onFileCreate(file)` for every TFile
//      in the vault — this is exactly what Obsidian's bootstrap loop
//      does during a real restart.
//   7. Wait for reconcile to settle.
//   8. Assert: no `.conflict-` files in the vault, plugin's manifest
//      size matches N (no phantom entries).

import { spawn, ChildProcess } from 'child_process';
import { join, relative } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline #58 — synthetic onFileCreate on vault load creates conflicts', () => {
    const port = 3058;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue58.db');
    const cliFolderPath = join(e2eDir, 'issue58-cli-vault');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    const N_FILES = 100;

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
        const files = new Map<string, string>();
        for (let i = 0; i < N_FILES; i++) {
            const seed = crypto.createHash('sha256').update(`#58-file-${i}`).digest('hex');
            const subdir = `dir-${(i % 5).toString().padStart(2, '0')}`;
            files.set(`${subdir}/note-${String(i).padStart(4, '0')}.md`, `# file ${i}\n\n${seed}\n`);
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

    it('cache-wipe + synthetic onFileCreate flood does not create conflicts', async function () {
        this.timeout(5 * 60_000);

        const vaultPath: string = await browser.executeObsidian(async ({ app }) => (app as any).vault.adapter.basePath as string);
        const corpus = makeCorpus();

        // Phase A: wait for initial convergence — vault has all N files,
        // plugin's manifest matches.
        await waitFor('initial convergence', async () => {
            const obs = listVault(vaultPath);
            for (const path of corpus.keys()) {
                if (!obs.has(path)) return false;
            }
            return true;
        }, 2 * 60_000, 200);

        const initialProjSize: number = await browser.executeObsidian(async ({ app }) => {
            const p: any = (app as any).plugins.plugins['syncline-obsidian'];
            return p?.lastProjection?.size ?? 0;
        });
        console.log(`[#58] phase A: initial projection size = ${initialProjSize}, expected ${N_FILES}`);
        expect(initialProjSize).toBe(N_FILES);

        // Phase B: disconnect, wipe manifest cache, keep vault files +
        // content cache (Obsidian still has the TFile tree).
        await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            plugin.disconnect();
        });
        const stateDir = join(vaultPath, '.obsidian', 'plugins', 'syncline-obsidian', 'v1');
        if (fs.existsSync(join(stateDir, 'manifest.bin'))) fs.unlinkSync(join(stateDir, 'manifest.bin'));
        if (fs.existsSync(join(stateDir, 'lamport.txt'))) fs.unlinkSync(join(stateDir, 'lamport.txt'));
        console.log(`[#58] phase B: manifest cache wiped (vault files retained)`);

        // Phase C: reconnect, wait for server's manifest STEP_2 to land
        // (so the LIVE manifest has the N paths), THEN fire synthetic
        // onFileCreate for every markdown file BEFORE the first
        // reconcile pass populates `lastProjection`. This is the exact
        // window the real-world bug reproduces in: server STEP_2 has
        // populated the live projection but the plugin's `lastProjection`
        // copy is still empty.
        await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            await plugin.connect();
        });
        // Wait for live manifest projection (queried straight off the
        // WASM client) to reach N. We deliberately do NOT wait for
        // `lastProjection` to update here — that would defeat the race.
        await waitFor('live projection populated', async () => {
            const liveSize: number = await browser.executeObsidian(async ({ app }) => {
                const p: any = (app as any).plugins.plugins['syncline-obsidian'];
                if (!p?.client) return 0;
                try {
                    return JSON.parse(p.client.projectionJson()).length;
                } catch { return 0; }
            });
            return liveSize >= N_FILES;
        }, 30_000, 50);

        // Block the manifestChanged-driven reconcile by stalling the JS
        // event loop's microtask handling… actually we can't easily do
        // that. Instead, fire the synthetic creates IMMEDIATELY in the
        // same executeObsidian call as a `lastProjection.clear()` so
        // the race window is wide:
        const result: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            // Force lastProjection back to empty to mimic the gap between
            // "server pushed manifest" and "plugin's first reconcile
            // populated lastProjection" during a real restart. Without
            // this, the plugin's first reconcile (kicked off by the
            // manifest STEP_2 we just waited for) may have already run
            // and populated lastProjection — wallpapering over the bug.
            plugin.lastProjection.clear();
            const allFiles = (app as any).vault.getMarkdownFiles();
            for (const f of allFiles) {
                plugin.onFileCreate(f);
            }
            return { firedCount: allFiles.length };
        });
        console.log(`[#58] phase C: live manifest populated, ${result.firedCount} synthetic onFileCreate fired`);

        // Phase D: wait for reconcile to settle (manifest size + ingestion
        // queue both stable).
        let lastProjSize = -1;
        let stableTicks = 0;
        await waitFor('reconcile settles', async () => {
            const sz: number = await browser.executeObsidian(async ({ app }) => {
                const p: any = (app as any).plugins.plugins['syncline-obsidian'];
                return p?.lastProjection?.size ?? 0;
            });
            if (sz === lastProjSize) stableTicks++; else stableTicks = 0;
            lastProjSize = sz;
            if (stableTicks % 5 === 0) console.log(`[#58] settle: projection size = ${sz}, stable=${stableTicks}`);
            return stableTicks >= 15;
        }, 3 * 60_000, 200);

        // Phase E: assert no conflict copies and projection size matches.
        const finalProjSize: number = await browser.executeObsidian(async ({ app }) => {
            const p: any = (app as any).plugins.plugins['syncline-obsidian'];
            return p?.lastProjection?.size ?? 0;
        });
        const obs = listVault(vaultPath);
        const conflicts: string[] = [...obs.keys()].filter((k) => k.includes('.conflict-'));
        const allMd: string[] = [...obs.keys()].filter((k) => k.endsWith('.md'));

        console.log(`[#58] phase E: projection=${finalProjSize}, vault_md=${allMd.length}, conflicts_on_disk=${conflicts.length}`);
        if (conflicts.length > 0) {
            console.error(`[#58] conflict files (sample):`);
            for (const c of conflicts.slice(0, 10)) console.error(`   ${c}`);
        }

        expect(conflicts.length).toBe(0);
        // Projection should not exceed N_FILES (any growth = conflict node was minted).
        expect(finalProjSize).toBeLessThanOrEqual(N_FILES);
    });
});
