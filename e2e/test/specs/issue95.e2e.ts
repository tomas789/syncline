// Regression test for #95 — device-local settings override.
//
// Pre-#95 every settings field — including `actorId` (this device's
// CRDT identity) — lived in `data.json`. If the user ever opted
// data.json into a sync surface (Obsidian Sync, LiveSync, syncline
// itself), actorId would propagate, two devices would share one,
// and Yrs history would corrupt permanently.
//
// #95 splits SynclineSettings into:
//   - SharedSettings (data.json):  serverUrl, autoSync, configSync
//   - LocalSettings  (localStorage): actorId
//
// What this test covers:
//   A. Partitioned save: actorId never appears in data.json after
//      saveSettings(); always lives in localStorage instead.
//   B. Migration: pre-#95 data.json with actorId → after onload
//      runs (or first saveSettings), actorId moves to localStorage,
//      data.json drops the field, in-memory state preserved.
//   C. Migration is idempotent: rerunning the migration path with
//      already-migrated state is a no-op.
//   D. Editing shared settings does not write actorId back into
//      data.json. Simulates the "device A propagates settings to
//      device B; device B's actorId stays untouched" scenario.
//   E. onExternalSettingsChange ignores any actorId field that
//      sneaks into data.json (e.g., from a buggy sync tool that
//      restored the legacy field). In-memory + localStorage actorId
//      stays put; data.json gets rewritten without it.

import { spawn, ChildProcess } from 'child_process';
import { join } from 'path';
import * as fs from 'fs';
import { expect, browser } from '@wdio/globals';

describe('Syncline #95 — device-local settings override', () => {
    const port = 3095;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue95.db');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    const LOCAL_LS_KEY = 'syncline:local-settings';

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
    });

    it('partitions, migrates, and refuses external actorId injection', async function () {
        this.timeout(2 * 60_000);

        // --------------------------------------------------------------
        // Phase A — partitioned save
        // --------------------------------------------------------------
        const phaseA: any = await browser.executeObsidian(async ({ app }, args) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            await plugin.saveSettings();
            const dataJson = await plugin.loadData();
            const localBlob = (app as any).loadLocalStorage(args.lsKey);
            return {
                actorIdInMemory: plugin.settings.actorId,
                actorIdInDataJson: dataJson?.actorId ?? null,
                actorIdInLocalStorage:
                    localBlob && typeof localBlob === 'object' ? localBlob.actorId ?? null : null,
                dataJsonKeys: dataJson ? Object.keys(dataJson).sort() : [],
            };
        }, { lsKey: LOCAL_LS_KEY });
        expect(typeof phaseA.actorIdInMemory).toBe('string');
        expect(phaseA.actorIdInDataJson).toBeNull(); // stripped from data.json
        expect(phaseA.actorIdInLocalStorage).toBe(phaseA.actorIdInMemory);
        expect(phaseA.dataJsonKeys).toEqual(['autoSync', 'configSync', 'serverUrl']);
        console.log(`[#95] phase A: actorId only in memory + localStorage; data.json keys = ${phaseA.dataJsonKeys.join(',')}`);

        // --------------------------------------------------------------
        // Phase B — pre-#95 migration: plant actorId back into data.json
        // and clear localStorage, then run the migration path
        // (loadSettings + the eager save in onload). Asserts:
        //   - actorId moves out of data.json
        //   - localStorage gets the actorId
        //   - in-memory actorId is preserved
        // --------------------------------------------------------------
        const phaseB: any = await browser.executeObsidian(async ({ app }, args) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const originalActorId = plugin.settings.actorId;

            // Set up the pre-#95 layout: actorId in data.json, nothing
            // in localStorage. We use saveData directly to bypass
            // saveSettings() (which would partition).
            await plugin.saveData({
                serverUrl: plugin.settings.serverUrl,
                autoSync: plugin.settings.autoSync,
                configSync: plugin.settings.configSync,
                actorId: originalActorId, // legacy placement
            });
            (app as any).saveLocalStorage(args.lsKey, null);

            // Trigger the migration path: loadSettings reads data.json,
            // marks deviceLocalMigrationNeeded, and the eager
            // saveSettings call (mirroring what onload does) partitions.
            await plugin.loadSettings();
            const migrationFlag = plugin.deviceLocalMigrationNeeded;
            if (plugin.deviceLocalMigrationNeeded) {
                await plugin.saveSettings();
                plugin.deviceLocalMigrationNeeded = false;
            }

            const dataJson = await plugin.loadData();
            const localBlob = (app as any).loadLocalStorage(args.lsKey);
            return {
                migrationFlagSet: migrationFlag,
                inMemoryActorId: plugin.settings.actorId,
                originalActorId,
                actorIdInDataJson: dataJson?.actorId ?? null,
                actorIdInLocalStorage:
                    localBlob && typeof localBlob === 'object' ? localBlob.actorId ?? null : null,
            };
        }, { lsKey: LOCAL_LS_KEY });
        expect(phaseB.migrationFlagSet).toBe(true);
        expect(phaseB.inMemoryActorId).toBe(phaseB.originalActorId);
        expect(phaseB.actorIdInDataJson).toBeNull();
        expect(phaseB.actorIdInLocalStorage).toBe(phaseB.originalActorId);
        console.log('[#95] phase B: migration moved actorId from data.json to localStorage');

        // --------------------------------------------------------------
        // Phase C — migration is idempotent
        // --------------------------------------------------------------
        const phaseC: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            // Re-running loadSettings on already-partitioned state.
            await plugin.loadSettings();
            return {
                migrationFlagSet: plugin.deviceLocalMigrationNeeded,
                inMemoryActorId: plugin.settings.actorId,
            };
        });
        expect(phaseC.migrationFlagSet).toBe(false); // localStorage is the source now
        expect(typeof phaseC.inMemoryActorId).toBe('string');
        console.log('[#95] phase C: migration idempotent (no flag, actorId preserved)');

        // --------------------------------------------------------------
        // Phase D — editing shared settings doesn't leak actorId.
        //   Mimics device A pushing a settings change; on device B
        //   the data.json that arrives must not contain actorId.
        // --------------------------------------------------------------
        const phaseD: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            // Touch a shared field, save.
            const before = plugin.settings.serverUrl;
            plugin.settings.serverUrl = before + '?probe=1';
            await plugin.saveSettings();
            const dataJson = await plugin.loadData();
            // Restore.
            plugin.settings.serverUrl = before;
            await plugin.saveSettings();
            return {
                hasActorIdField: dataJson
                    ? Object.prototype.hasOwnProperty.call(dataJson, 'actorId')
                    : false,
                serverUrlInDataJson: dataJson?.serverUrl,
            };
        });
        expect(phaseD.hasActorIdField).toBe(false);
        expect(typeof phaseD.serverUrlInDataJson).toBe('string');
        console.log('[#95] phase D: shared-settings edit does not leak actorId into data.json');

        // --------------------------------------------------------------
        // Phase E — onExternalSettingsChange refuses to honor an
        //   external actorId injection. Plant a stale actorId into
        //   data.json (as if a buggy sync tool restored the legacy
        //   field), trigger the hook, confirm:
        //     - in-memory actorId unchanged
        //     - localStorage actorId unchanged
        //     - data.json rewritten without actorId
        // --------------------------------------------------------------
        const phaseE: any = await browser.executeObsidian(async ({ app }, args) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const ourActor = plugin.settings.actorId;
            const stalActor = '00000000-0000-4000-8000-deadbeefcafe';

            // Inject the stale actorId via the adapter (so loadData()
            // sees the new bytes coherently — same reason as the #90
            // test).
            const adapter = (app as any).vault.adapter;
            const cd = (app as any).vault.configDir;
            const path = `${cd}/plugins/syncline/data.json`;
            const cur = JSON.parse(await adapter.read(path));
            cur.actorId = stalActor;
            await adapter.write(path, JSON.stringify(cur));

            // Bypass the self-write cookie so the hook actually runs
            // (the rapid-fire test cadence trips the 250 ms guard).
            plugin.lastSelfSaveAt = 0;
            await plugin.onExternalSettingsChange();

            const dataJson = await plugin.loadData();
            const localBlob = (app as any).loadLocalStorage(args.lsKey);
            return {
                inMemoryActorId: plugin.settings.actorId,
                ourActor,
                actorIdInDataJson: dataJson?.actorId ?? null,
                actorIdInLocalStorage:
                    localBlob && typeof localBlob === 'object' ? localBlob.actorId ?? null : null,
            };
        }, { lsKey: LOCAL_LS_KEY });
        expect(phaseE.inMemoryActorId).toBe(phaseE.ourActor);
        expect(phaseE.actorIdInLocalStorage).toBe(phaseE.ourActor);
        expect(phaseE.actorIdInDataJson).toBeNull(); // stripped on rewrite
        console.log('[#95] phase E: external actorId injection rejected; data.json sanitized');
    });
});
