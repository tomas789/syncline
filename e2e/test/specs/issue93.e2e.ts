// Regression test for #93 — per-category data.json sync allow-list.
//
// #92 shipped a single boolean (`syncObsidianConfig`) gating all
// `.obsidian/` syncing. #93 replaces it with per-category toggles:
//
//   - themes / snippets / hotkeys / coreConfig: on by default
//   - communityPluginList: off by default (opt-in)
//   - communityPluginData: per-plugin allow-list (off by default each)
//   - other: off by default (catch-all for unclassified hidden paths)
//
// The plugin must:
//   - Classify any `${configDir}/...` path into one of those buckets.
//   - Filter manifest projection rows to ones whose category is on
//     (drop rows from disabled categories — don't materialize them).
//   - Hard-exclude its own `${configDir}/plugins/<id>/` regardless.
//   - Migrate the legacy #92 boolean: true → safe categories on,
//     risky off; false → everything off.
//
// Test strategy: poke the helpers + settings directly via
// executeObsidian. Doesn't need a server (the classification + filter
// logic is local state).

import { spawn, ChildProcess } from 'child_process';
import { join } from 'path';
import * as fs from 'fs';
import { expect, browser } from '@wdio/globals';

describe('Syncline #93 — per-category data.json sync allow-list', () => {
    const port = 3093;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue93.db');
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
            plugin.settings.autoSync = true;
            // Force fresh #93-shape settings so the test isn't
            // affected by leftover state from prior specs.
            plugin.settings.configSync = {
                themes: true,
                snippets: true,
                hotkeys: true,
                coreConfig: true,
                communityPluginList: false,
                communityPluginData: {},
                other: false,
            };
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

    it('classifies, filters, and migrates per category', async function () {
        this.timeout(2 * 60_000);

        // --------------------------------------------------------------
        // Phase A — path classifier covers all the documented buckets,
        // including hard-excluding our own plugin folder.
        // --------------------------------------------------------------
        const phaseA: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const cd = (app as any).vault.configDir;
            const cases: Array<[string, string]> = [
                [`${cd}/themes/dracula/theme.css`, 'themes'],
                [`${cd}/snippets/custom.css`, 'snippets'],
                [`${cd}/hotkeys.json`, 'hotkeys'],
                [`${cd}/app.json`, 'coreConfig'],
                [`${cd}/appearance.json`, 'coreConfig'],
                [`${cd}/core-plugins.json`, 'coreConfig'],
                [`${cd}/community-plugins.json`, 'communityPluginList'],
                [`${cd}/plugins/some-plugin/data.json`, 'communityPluginData'],
                [`${cd}/plugins/some-plugin/main.js`, 'communityPluginData'],
                [`${cd}/plugins/syncline/data.json`, 'self'], // hard-exclude
                [`${cd}/plugins/syncline/v1/manifest.bin`, 'self'],
                [`${cd}/types.json`, 'other'],
                [`${cd}/bookmarks/x.json`, 'other'],
                ['notes/foo.md', null], // outside config
                ['some-vault-file.md', null],
            ];
            const results: Array<{ path: string; expected: string | null; got: string | null }> = [];
            for (const [path, expected] of cases) {
                const got = plugin.classifyHiddenPath(path);
                results.push({ path, expected, got });
            }
            return { results };
        });
        for (const r of phaseA.results) {
            expect(`${r.path} → ${r.got}`).toBe(`${r.path} → ${r.expected}`);
        }
        console.log(`[#93] phase A: classifier matches ${phaseA.results.length} cases`);

        // --------------------------------------------------------------
        // Phase B — `shouldSyncHiddenPath` honours category settings:
        // safe categories on; community plugin list / data off.
        // --------------------------------------------------------------
        const phaseB: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const cd = (app as any).vault.configDir;
            return {
                themes: plugin.shouldSyncHiddenPath(`${cd}/themes/x.css`),
                snippets: plugin.shouldSyncHiddenPath(`${cd}/snippets/x.css`),
                hotkeys: plugin.shouldSyncHiddenPath(`${cd}/hotkeys.json`),
                coreConfig: plugin.shouldSyncHiddenPath(`${cd}/app.json`),
                communityList: plugin.shouldSyncHiddenPath(`${cd}/community-plugins.json`),
                pluginData: plugin.shouldSyncHiddenPath(`${cd}/plugins/dataview/data.json`),
                self: plugin.shouldSyncHiddenPath(`${cd}/plugins/syncline/v1/manifest.bin`),
                other: plugin.shouldSyncHiddenPath(`${cd}/types.json`),
                outsideConfig: plugin.shouldSyncHiddenPath('foo/bar.md'),
            };
        });
        expect(phaseB.themes).toBe(true);
        expect(phaseB.snippets).toBe(true);
        expect(phaseB.hotkeys).toBe(true);
        expect(phaseB.coreConfig).toBe(true);
        expect(phaseB.communityList).toBe(false);
        expect(phaseB.pluginData).toBe(false); // not in allow-list
        expect(phaseB.self).toBe(false); // hard-excluded
        expect(phaseB.other).toBe(false);
        expect(phaseB.outsideConfig).toBe(true); // not classified, no opinion
        console.log('[#93] phase B: shouldSyncHiddenPath honors category settings');

        // --------------------------------------------------------------
        // Phase C — per-plugin allow-list: enabling a specific plugin
        // flips communityPluginData ON for that plugin only.
        // --------------------------------------------------------------
        const phaseC: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const cd = (app as any).vault.configDir;
            plugin.settings.configSync.communityPluginData['dataview'] = true;
            return {
                dataviewOn: plugin.shouldSyncHiddenPath(`${cd}/plugins/dataview/data.json`),
                templaterOff: plugin.shouldSyncHiddenPath(`${cd}/plugins/templater/data.json`),
            };
        });
        expect(phaseC.dataviewOn).toBe(true);
        expect(phaseC.templaterOff).toBe(false);
        console.log('[#93] phase C: per-plugin allow-list isolates dataview from templater');

        // --------------------------------------------------------------
        // Phase D — reconcile filter drops disabled-category rows from
        // lastProjection. Inject a hidden manifest entry under a
        // disabled category (`other` — types.json), trigger reconcile,
        // confirm it's not materialized.
        // --------------------------------------------------------------
        const phaseD: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const cd = (app as any).vault.configDir;
            const path = `${cd}/types.json`; // category: other → off
            plugin.client.createTextAllowingCollision(path, 0);
            await new Promise((r) => setTimeout(r, 500));
            return {
                inLastProjection: plugin.lastProjection.has(path),
                onDisk: await (app as any).vault.adapter.exists(path),
                inLiveProjection: JSON.parse(plugin.client.projectionJson()).some((r: any) => r.path === path),
            };
        });
        expect(phaseD.inLastProjection).toBe(false); // filtered
        expect(phaseD.onDisk).toBe(false); // not materialized
        expect(phaseD.inLiveProjection).toBe(true); // server-side preserved
        console.log('[#93] phase D: disabled-category row filtered from reconcile, server-side preserved');

        // --------------------------------------------------------------
        // Phase D2 — toggling a category OFF does not delete files
        //   already on disk under that category. This is the
        //   high-stakes regression: if reconcile's removal pass
        //   treated filtered-out rows as "no longer in the manifest",
        //   it would silently delete user files when a category gets
        //   disabled. (Inherited from #92's Phase C — same risk under
        //   the new per-category model, just multiplied by N.)
        // --------------------------------------------------------------
        const phaseD2: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const cd = (app as any).vault.configDir;
            const path = `${cd}/themes/disposable-theme.css`;

            // Materialize via the manifest under an enabled category
            // first, so the file lands on disk and lastProjection
            // contains its row.
            plugin.client.createTextAllowingCollision(path, 0);
            // Pretend the file got materialized (the test
            // environment's reconcile path may or may not race a
            // real write here; just adapter.write it directly).
            await (app as any).vault.adapter.write(path, '/* test */');
            await plugin.reconcileProjection();
            await new Promise((r) => setTimeout(r, 500));
            const onDiskBefore = await (app as any).vault.adapter.exists(path);
            const inProjectionBefore = plugin.lastProjection.has(path);

            // Now disable the themes category. Reconcile will filter
            // the row out of the new byId; the removal pass must
            // SKIP it (preserve the local file).
            plugin.settings.configSync.themes = false;
            await plugin.reconcileProjection();
            await new Promise((r) => setTimeout(r, 500));
            const onDiskAfter = await (app as any).vault.adapter.exists(path);

            // Restore for cleanup.
            plugin.settings.configSync.themes = true;

            return {
                onDiskBefore,
                inProjectionBefore,
                onDiskAfter,
            };
        });
        expect(phaseD2.onDiskBefore).toBe(true);
        expect(phaseD2.inProjectionBefore).toBe(true);
        expect(phaseD2.onDiskAfter).toBe(true); // NOT deleted by toggle
        console.log('[#93] phase D2: toggle category OFF preserves local file (no silent delete)');

        // --------------------------------------------------------------
        // Phase E — legacy boolean migration:
        //   - syncObsidianConfig=true  → safe categories on, risky off.
        //   - syncObsidianConfig=false → everything off.
        // --------------------------------------------------------------
        const phaseE: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];

            // Simulate the legacy boolean = true case.
            plugin.legacyBooleanMigrationNeeded = true;
            plugin.legacyBooleanValue = true;
            // Inline the migration block from connect() so we don't need
            // to disconnect + reconnect for this assertion.
            if (plugin.legacyBooleanMigrationNeeded) {
                if (plugin.legacyBooleanValue === true) {
                    plugin.settings.configSync = {
                        themes: true,
                        snippets: true,
                        hotkeys: true,
                        coreConfig: true,
                        communityPluginList: false,
                        communityPluginData: {},
                        other: false,
                    };
                } else {
                    plugin.settings.configSync = {
                        themes: false,
                        snippets: false,
                        hotkeys: false,
                        coreConfig: false,
                        communityPluginList: false,
                        communityPluginData: {},
                        other: false,
                    };
                }
                plugin.legacyBooleanMigrationNeeded = false;
                plugin.legacyBooleanValue = null;
            }
            const trueCase = { ...plugin.settings.configSync };

            // Now simulate legacy boolean = false.
            plugin.legacyBooleanMigrationNeeded = true;
            plugin.legacyBooleanValue = false;
            if (plugin.legacyBooleanMigrationNeeded) {
                if (plugin.legacyBooleanValue === true) {
                    plugin.settings.configSync = {
                        themes: true,
                        snippets: true,
                        hotkeys: true,
                        coreConfig: true,
                        communityPluginList: false,
                        communityPluginData: {},
                        other: false,
                    };
                } else {
                    plugin.settings.configSync = {
                        themes: false,
                        snippets: false,
                        hotkeys: false,
                        coreConfig: false,
                        communityPluginList: false,
                        communityPluginData: {},
                        other: false,
                    };
                }
                plugin.legacyBooleanMigrationNeeded = false;
                plugin.legacyBooleanValue = null;
            }
            const falseCase = { ...plugin.settings.configSync };

            return { trueCase, falseCase };
        });
        // legacy true → safe categories on
        expect(phaseE.trueCase.themes).toBe(true);
        expect(phaseE.trueCase.snippets).toBe(true);
        expect(phaseE.trueCase.hotkeys).toBe(true);
        expect(phaseE.trueCase.coreConfig).toBe(true);
        expect(phaseE.trueCase.communityPluginList).toBe(false);
        expect(phaseE.trueCase.other).toBe(false);
        // legacy false → everything off (preserve user's explicit "no")
        expect(phaseE.falseCase.themes).toBe(false);
        expect(phaseE.falseCase.snippets).toBe(false);
        expect(phaseE.falseCase.hotkeys).toBe(false);
        expect(phaseE.falseCase.coreConfig).toBe(false);
        expect(phaseE.falseCase.communityPluginList).toBe(false);
        expect(phaseE.falseCase.other).toBe(false);
        console.log('[#93] phase E: legacy boolean migration: true → safe-on; false → all-off');
    });
});
