// Regression test for #56 — case-insensitive FS path collisions create
// phantom manifest entries.
//
// On case-insensitive filesystems (macOS APFS default, Windows NTFS
// default), two manifest paths that differ only in case fold to the
// same disk file. Walking the vault back to look for "new" files then
// finds the disk path under whatever case the FS preserved (often
// different from the manifest's case), and `pathsInManifest.has(file.path)`
// returns false because the lookup is byte-equal. The plugin then
// uploads the disk file as a new manifest entry — silently growing
// the manifest by one phantom row on every reconnect.
//
// This test isolates the surface in `scanLocalVault`:
//   1. Pre-create a vault folder with one case (`CaseTest/`) and a
//      file inside (`CaseTest/foo.md`).
//   2. Inject a manifest entry at the OTHER case (`casetest/foo.md`)
//      via the WASM client — same shape a Linux peer's STEP_2 would
//      produce.
//   3. Run `plugin.scanLocalVault()`.
//   4. Assert the manifest didn't grow — the disk path matches the
//      manifest entry under case-insensitive comparison.
//
// On the buggy code, scanLocalVault finds `CaseTest/foo.md` is not in
// `pathsInManifest` (the set has lowercase `casetest/foo.md`) and
// creates a phantom node, growing the projection by 1 row.

import { spawn, ChildProcess } from 'child_process';
import { join } from 'path';
import * as fs from 'fs';
import { expect, browser } from '@wdio/globals';

describe('Syncline #56 — case-insensitive FS phantom manifest entries', () => {
    const port = 3059;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue56.db');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    let serverProc: ChildProcess;

    async function waitFor<T>(label: string, fn: () => Promise<T | null | undefined | false | ''>, timeoutMs = 30_000, stepMs = 100): Promise<T> {
        const deadline = Date.now() + timeoutMs;
        while (Date.now() < deadline) {
            try { const v = await fn(); if (v) return v as T; } catch {}
            await new Promise((r) => setTimeout(r, stepMs));
        }
        throw new Error(`waitFor("${label}") timed out after ${timeoutMs}ms`);
    }

    before(async function () {
        this.timeout(2 * 60_000);
        if (!fs.existsSync(synclineBin)) throw new Error(`syncline binary missing at ${synclineBin}`);
        if (fs.existsSync(dbPath)) fs.unlinkSync(dbPath);

        let serverOut = '';
        serverProc = spawn(synclineBin, ['server', '--port', String(port), '--db-path', dbPath], { stdio: 'pipe' });
        serverProc.stdout?.on('data', (d) => { serverOut += d; });
        serverProc.stderr?.on('data', (d) => { serverOut += d; });
        await waitFor('server listening', async () => /listening/i.test(serverOut), 30_000, 100);

        const obsidianPage = browser.getObsidianPage();
        try { await obsidianPage.enablePlugin('syncline'); } catch {}
        await browser.executeObsidian(async ({ app }, url) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            if (!plugin) throw new Error('plugin not found');
            plugin.settings.serverUrl = url;
            await plugin.saveSettings();
            plugin.disconnect();
            await plugin.connect();
        }, serverUrl);
        await waitFor('plugin connected', async () => browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            return !!(plugin && plugin.client && plugin.client.isConnected());
        }), 30_000, 100);
    });

    after(() => {
        if (serverProc && !serverProc.killed) serverProc.kill();
    });

    it('scanLocalVault does not phantom-upload a case-folded disk path', async function () {
        this.timeout(60_000);

        // Reset to a clean state: manifest empty (server-side has no
        // entries; we'll inject locally). Vault: drop our test folder
        // with the mixed-case name FIRST so that subsequent
        // case-insensitive lookups resolve to it.
        const result: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const adapter = (app as any).vault.adapter;

            // Clean any prior bench-foo dirs.
            for (const cand of ['CaseTest', 'casetest']) {
                if (await adapter.exists(cand)) {
                    // recursive remove
                    const stack = [cand];
                    while (stack.length) {
                        const p = stack.pop()!;
                        try {
                            const entries = await adapter.list(p);
                            for (const f of entries.files) await adapter.remove(f);
                            for (const d of entries.folders) stack.push(d);
                        } catch {}
                    }
                    try { await adapter.rmdir(cand, true); } catch {}
                }
            }

            // Create folder with mixed case.
            await (app as any).vault.createFolder('CaseTest');
            await (app as any).vault.create('CaseTest/foo.md', 'hello world');

            // Inject a manifest entry at the LOWERCASE form — same shape
            // a Linux peer would produce.
            plugin.client.createTextAllowingCollision('casetest/foo.md', 11);

            const beforeSize = JSON.parse(plugin.client.projectionJson()).length;

            // Run scanLocalVault — this is the surface that reads the
            // vault's getFiles() and looks them up in the projection.
            await plugin.scanLocalVault();

            const afterSize = JSON.parse(plugin.client.projectionJson()).length;
            const projection = JSON.parse(plugin.client.projectionJson());

            return {
                beforeSize,
                afterSize,
                paths: projection.map((r: any) => r.path),
            };
        });

        console.log(`[#56] projection size: before=${result.beforeSize}, after=${result.afterSize}`);
        console.log(`[#56] projection paths: ${JSON.stringify(result.paths)}`);

        // Bug fingerprint: scanLocalVault saw the disk path under a case
        // the manifest didn't have, treated it as new, and uploaded a
        // phantom entry → afterSize > beforeSize.
        expect(result.afterSize).toBe(result.beforeSize);
    });
});
