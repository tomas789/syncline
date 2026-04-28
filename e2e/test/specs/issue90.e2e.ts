// Regression test for #90 — onExternalSettingsChange.
//
// Obsidian fires this hook when our `data.json` is rewritten by something
// other than this plugin instance — typically a second sync tool, or the
// user editing the file by hand. The plugin must:
//   - Pick up `serverUrl` and `autoSync` changes without restart.
//   - REFUSE to overwrite `actorId`. ActorId is this device's CRDT
//     identity; if two devices ever share one, Yrs history is corrupted
//     permanently. On mismatch, keep ours and rewrite `data.json` so the
//     on-disk state matches reality.
//   - Ignore the echo of our own `saveSettings()` writes (250 ms cookie).
//
// Test strategy: write `data.json` via Obsidian's vault adapter (NOT
// `Plugin.saveData`, so it counts as "external" from the plugin's
// perspective), then invoke `plugin.onExternalSettingsChange()`
// directly. We bypass Obsidian's own fs-watcher (unreliable under
// xvfb / headless chrome) and bypass Node-side `fs.writeFileSync`
// (the renderer's vault adapter may not see external fs writes
// coherently in CI). Tests the plugin's logic deterministically.

import { spawn, ChildProcess } from 'child_process';
import { join } from 'path';
import * as fs from 'fs';
import { expect, browser } from '@wdio/globals';

describe('Syncline #90 — onExternalSettingsChange', () => {
    const port = 3090;
    const altPort = 3091;
    const serverUrl = `ws://localhost:${port}/sync`;
    const altServerUrl = `ws://localhost:${altPort}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue90.db');
    const altDbPath = join(e2eDir, 'issue90-alt.db');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    let serverProc: ChildProcess;
    let altServerProc: ChildProcess;

    async function waitFor<T>(label: string, fn: () => Promise<T | null | undefined | false | ''>, timeoutMs = 30_000, stepMs = 100): Promise<T> {
        const deadline = Date.now() + timeoutMs;
        while (Date.now() < deadline) {
            try { const v = await fn(); if (v) return v as T; } catch {}
            await new Promise((r) => setTimeout(r, stepMs));
        }
        throw new Error(`waitFor("${label}") timed out after ${timeoutMs}ms`);
    }

    async function startServer(serverPort: number, db: string): Promise<ChildProcess> {
        let out = '';
        const proc = spawn(synclineBin, ['server', '--port', String(serverPort), '--db-path', db], { stdio: 'pipe' });
        proc.stdout?.on('data', (d) => { out += d; });
        proc.stderr?.on('data', (d) => { out += d; });
        await waitFor(`server :${serverPort} listening`, async () => /listening/i.test(out), 30_000, 100);
        return proc;
    }

    /**
     * Mutate data.json via Obsidian's vault adapter — equivalent to what
     * another in-Obsidian plugin or another adapter-based sync tool
     * would do. Counts as "external" from this plugin's perspective
     * (does NOT go through `this.saveData`), but stays adapter-coherent
     * so the next `loadData()` call sees the new bytes immediately.
     *
     * Returns nothing; it's run inside Obsidian via executeObsidian.
     */
    async function adapterRewriteDataJson(mutation: { kind: 'serverUrl' | 'autoSync' | 'actorId'; value: any }) {
        await browser.executeObsidian(async ({ app }, m) => {
            const adapter = (app as any).vault.adapter;
            const cd = (app as any).vault.configDir;
            const path = `${cd}/plugins/syncline/data.json`;
            const cur = JSON.parse(await adapter.read(path));
            cur[m.kind] = m.value;
            await adapter.write(path, JSON.stringify(cur));
        }, mutation);
    }

    before(async function () {
        this.timeout(2 * 60_000);
        if (!fs.existsSync(synclineBin)) throw new Error(`syncline binary missing at ${synclineBin}`);
        for (const p of [dbPath, altDbPath]) {
            if (fs.existsSync(p)) fs.unlinkSync(p);
        }

        // Two servers so the URL-swap phase reaches a real handshake on
        // both ends — otherwise the post-swap connect() leaves the plugin
        // in "connecting…" forever and subsequent phases race against
        // background reconnect attempts.
        serverProc = await startServer(port, dbPath);
        altServerProc = await startServer(altPort, altDbPath);

        const obsidianPage = browser.getObsidianPage();
        try { await obsidianPage.enablePlugin('syncline'); } catch {}
        await browser.executeObsidian(async ({ app }, url) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            if (!plugin) throw new Error('plugin not found');
            plugin.settings.serverUrl = url;
            plugin.settings.autoSync = true;
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
        if (altServerProc && !altServerProc.killed) altServerProc.kill();
    });

    it('reacts to external data.json changes correctly', async function () {
        this.timeout(2 * 60_000);

        const originalActorId: string = await browser.executeObsidian(async ({ app }) => {
            const adapter = (app as any).vault.adapter;
            const cd = (app as any).vault.configDir;
            const path = `${cd}/plugins/syncline/data.json`;
            const exists = await adapter.exists(path);
            if (!exists) throw new Error(`data.json missing at ${path}`);
            const cur = JSON.parse(await adapter.read(path));
            return cur.actorId;
        });
        if (!originalActorId) throw new Error('plugin has no actorId yet — connect before this test');

        // --------------------------------------------------------------
        // Phase A — serverUrl change picks up; client is reinstantiated.
        // --------------------------------------------------------------
        // Reset the self-write cookie so the hook's cookie guard (which
        // exists to filter out the echo of our own saveSettings()) does
        // NOT early-return. The before() block calls saveSettings within
        // its setup; on a fast machine the test reaches the hook well
        // within the 250 ms cookie window. Production behavior is
        // tested separately in Phase D.
        await adapterRewriteDataJson({ kind: 'serverUrl', value: altServerUrl });
        const phaseA: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            plugin.lastSelfSaveAt = 0;
            // Diagnostic: confirm the on-disk write is visible to
            // loadData() before we trigger the hook. If raw is null
            // here, the failure is at the IO layer, not the hook logic.
            const raw = await plugin.loadData();
            const oldClient = plugin.client;
            await plugin.onExternalSettingsChange();
            return {
                rawSeenByLoadData: raw,
                settingsUrl: plugin.settings.serverUrl,
                clientReinstantiated: plugin.client !== oldClient,
                clientNotNull: !!plugin.client,
            };
        });
        console.log(`[#90] phase A diagnostic: loadData saw serverUrl=${phaseA.rawSeenByLoadData?.serverUrl}`);
        expect(phaseA.rawSeenByLoadData?.serverUrl).toBe(altServerUrl);
        expect(phaseA.settingsUrl).toBe(altServerUrl);
        expect(phaseA.clientReinstantiated).toBe(true);
        expect(phaseA.clientNotNull).toBe(true);
        await waitFor('plugin reconnected to alt', async () => browser.executeObsidian(async ({ app }) => {
            const p: any = (app as any).plugins.plugins['syncline'];
            return !!(p && p.client && p.client.isConnected());
        }), 30_000, 100);
        console.log(`[#90] phase A: serverUrl change reflected, client reinstantiated, reconnected to ${altServerUrl}`);

        // Restore back to primary server for subsequent phases.
        await adapterRewriteDataJson({ kind: 'serverUrl', value: serverUrl });
        await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            plugin.lastSelfSaveAt = 0;
            await plugin.onExternalSettingsChange();
        });
        await waitFor('plugin reconnected to primary', async () => browser.executeObsidian(async ({ app }) => {
            const p: any = (app as any).plugins.plugins['syncline'];
            return !!(p && p.client && p.client.isConnected());
        }), 30_000, 100);

        // --------------------------------------------------------------
        // Phase B — autoSync: false disconnects the client.
        // --------------------------------------------------------------
        await adapterRewriteDataJson({ kind: 'autoSync', value: false });
        const phaseB: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            plugin.lastSelfSaveAt = 0;
            await plugin.onExternalSettingsChange();
            return {
                clientNull: plugin.client === null,
                autoSync: plugin.settings.autoSync,
            };
        });
        expect(phaseB.autoSync).toBe(false);
        expect(phaseB.clientNull).toBe(true);
        console.log(`[#90] phase B: autoSync=false disconnected client`);

        // Restore autoSync, reconnect.
        await adapterRewriteDataJson({ kind: 'autoSync', value: true });
        await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            plugin.lastSelfSaveAt = 0;
            await plugin.onExternalSettingsChange();
        });
        await waitFor('plugin reconnected (post-B)', async () => browser.executeObsidian(async ({ app }) => {
            const p: any = (app as any).plugins.plugins['syncline'];
            return !!(p && p.client && p.client.isConnected());
        }), 30_000, 100);

        // --------------------------------------------------------------
        // Phase C — actorId mismatch is REJECTED; data.json is restored.
        //   This is the highest-stakes assertion: silently accepting an
        //   external actorId would corrupt Yrs history across devices.
        // --------------------------------------------------------------
        const fakeActorId = '00000000-0000-4000-8000-deadbeefcafe';
        await adapterRewriteDataJson({ kind: 'actorId', value: fakeActorId });
        const phaseC: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            plugin.lastSelfSaveAt = 0;
            await plugin.onExternalSettingsChange();
            const adapter = (app as any).vault.adapter;
            const cd = (app as any).vault.configDir;
            const path = `${cd}/plugins/syncline/data.json`;
            const onDisk = JSON.parse(await adapter.read(path));
            return {
                inMemoryActor: plugin.settings.actorId,
                onDiskActor: onDisk.actorId,
            };
        });
        expect(phaseC.inMemoryActor).toBe(originalActorId);
        expect(phaseC.onDiskActor).toBe(originalActorId);
        console.log(`[#90] phase C: actorId mismatch rejected; on-disk file restored`);

        // --------------------------------------------------------------
        // Phase D — self-write echo within the cookie window is ignored.
        //   saveSettings() refreshes lastSelfSaveAt; an immediate
        //   onExternalSettingsChange() call must early-return without
        //   touching client state.
        // --------------------------------------------------------------
        const phaseD: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const beforeUrl = plugin.settings.serverUrl;
            const beforeClient = plugin.client;
            const beforeAutoSync = plugin.settings.autoSync;
            await plugin.saveSettings();
            await plugin.onExternalSettingsChange();
            return {
                urlSame: plugin.settings.serverUrl === beforeUrl,
                clientSame: plugin.client === beforeClient,
                autoSyncSame: plugin.settings.autoSync === beforeAutoSync,
            };
        });
        expect(phaseD.urlSame).toBe(true);
        expect(phaseD.clientSame).toBe(true);
        expect(phaseD.autoSyncSame).toBe(true);
        console.log(`[#90] phase D: self-write echo did not trigger reaction`);
    });
});
