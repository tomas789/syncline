// Regression test for #94 — relocate plugin state to IndexedDB.
//
// Pre-#94 the plugin wrote its CRDT state cache to
// `${configDir}/plugins/syncline/v1/{manifest.bin,lamport.txt,content/<id>.bin}`.
// That made the state visible to the hidden-file scanner, which was
// patched over with a self-exclusion ignore rule (Tier 1) and a
// hash-equality write guard (Tier 1) and a self-write fs-event
// cookie (#91). All three were band-aids.
//
// #94 moves the state into IndexedDB (manifest + content) and
// localStorage (lamport, vault id) — invisible to the vault adapter
// and therefore physically un-watchable by the scanner. Settings
// stay in `data.json` (still expected behavior for an Obsidian
// plugin).
//
// Asserts:
//   A. Storage roundtrip: setManifest/getManifest, putContent/
//      getContent/deleteContent, setLamport/getLamport.
//   B. Fresh install: no on-disk state files are created during
//      normal operation; the v1/ directory does not appear under
//      the plugin folder.
//   C. Migration: pre-existing on-disk state files are read into
//      IndexedDB on plugin load and removed from disk. Idempotent
//      (a second migration pass with no on-disk files is a no-op).
//   D. Defense-in-depth: ingestOrSyncHiddenFile, given a path inside
//      the plugin's own folder, refuses to upload and logs an error.
//      (The ignore-pattern logic should keep us out of the folder
//      entirely; this is the backup if that ever regresses.)

import { spawn, ChildProcess } from 'child_process';
import { join } from 'path';
import * as fs from 'fs';
import { expect, browser } from '@wdio/globals';

describe('Syncline #94 — state in IndexedDB', () => {
    const port = 3094;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue94.db');
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

    it('storage roundtrip + fresh install + migration + defense-in-depth', async function () {
        this.timeout(2 * 60_000);

        // --------------------------------------------------------------
        // Phase A — storage roundtrip
        // --------------------------------------------------------------
        const phaseA: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];

            // Manifest
            const m1 = new Uint8Array([1, 2, 3, 4, 5]);
            await plugin.storage.setManifest(m1);
            const m1Back = await plugin.storage.getManifest();

            // Content
            await plugin.storage.putContent('test-node', new Uint8Array([9, 8, 7]));
            const c1 = await plugin.storage.getContent('test-node');
            await plugin.storage.deleteContent('test-node');
            const c2 = await plugin.storage.getContent('test-node');

            // Lamport
            plugin.storage.setLamport(42);
            const lamp = plugin.storage.getLamport();

            return {
                manifestEqual:
                    !!m1Back &&
                    m1Back.length === m1.length &&
                    m1Back.every((b: number, i: number) => b === m1[i]),
                contentRoundtripped:
                    !!c1 && c1.length === 3 && c1[0] === 9 && c1[1] === 8 && c1[2] === 7,
                contentDeleted: c2 === null,
                lamport: lamp,
            };
        });
        expect(phaseA.manifestEqual).toBe(true);
        expect(phaseA.contentRoundtripped).toBe(true);
        expect(phaseA.contentDeleted).toBe(true);
        expect(phaseA.lamport).toBe(42);
        console.log('[#94] phase A: storage roundtrip ok');

        // --------------------------------------------------------------
        // Phase B — fresh install: no on-disk state files
        //   The plugin has been running. Confirm the on-disk v1/ root
        //   was never created (or has been migrated away).
        // --------------------------------------------------------------
        const phaseB: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const adapter = (app as any).vault.adapter;
            const cd = (app as any).vault.configDir;
            const v1Root = `${cd}/plugins/${plugin.manifest.id}/v1`;
            const manifestPath = `${v1Root}/manifest.bin`;
            const lamportPath = `${v1Root}/lamport.txt`;
            const contentDir = `${v1Root}/content`;
            return {
                v1Exists: await adapter.exists(v1Root),
                manifestOnDisk: await adapter.exists(manifestPath),
                lamportOnDisk: await adapter.exists(lamportPath),
                contentDirOnDisk: await adapter.exists(contentDir),
            };
        });
        expect(phaseB.v1Exists).toBe(false);
        expect(phaseB.manifestOnDisk).toBe(false);
        expect(phaseB.lamportOnDisk).toBe(false);
        expect(phaseB.contentDirOnDisk).toBe(false);
        console.log('[#94] phase B: no on-disk state files for a fresh install');

        // --------------------------------------------------------------
        // Phase C — migration: write fake on-disk state files, run the
        //   migration directly, confirm files end up in IndexedDB and
        //   on-disk artifacts are removed.
        // --------------------------------------------------------------
        const phaseC: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const adapter = (app as any).vault.adapter;
            const cd = (app as any).vault.configDir;
            const v1Root = `${cd}/plugins/${plugin.manifest.id}/v1`;
            const manifestPath = `${v1Root}/manifest.bin`;
            const lamportPath = `${v1Root}/lamport.txt`;
            const contentDir = `${v1Root}/content`;
            const contentFile = `${contentDir}/legacy-node.bin`;

            // Save current storage state so we can restore at the end.
            const savedManifest = await plugin.storage.getManifest();
            const savedLamport = plugin.storage.getLamport();
            // Wipe storage to give the migration a clean target.
            await plugin.storage.setManifest(new Uint8Array(0));
            plugin.storage.setLamport(0);

            // Plant fake on-disk state.
            await adapter.mkdir(v1Root);
            await adapter.mkdir(contentDir);
            const manifestBytes = new Uint8Array([10, 20, 30, 40, 50]);
            await adapter.writeBinary(
                manifestPath,
                manifestBytes.buffer.slice(0) as ArrayBuffer,
            );
            await adapter.write(lamportPath, '7');
            const contentBytes = new Uint8Array([100, 101, 102]);
            await adapter.writeBinary(
                contentFile,
                contentBytes.buffer.slice(0) as ArrayBuffer,
            );

            // Run the migration directly.
            await plugin.migrateOnDiskStateToIndexedDB();

            const result = {
                manifestInStorage: await plugin.storage.getManifest(),
                lamportInStorage: plugin.storage.getLamport(),
                legacyContentInStorage: await plugin.storage.getContent('legacy-node'),
                manifestPathStillOnDisk: await adapter.exists(manifestPath),
                lamportPathStillOnDisk: await adapter.exists(lamportPath),
                contentFileStillOnDisk: await adapter.exists(contentFile),
                v1RootStillOnDisk: await adapter.exists(v1Root),
            };

            // Idempotency check: a second migration should be a no-op.
            await plugin.migrateOnDiskStateToIndexedDB();

            // Restore prior storage state for subsequent phases /
            // tests. Don't restore on-disk files — the migration
            // already cleaned them up and that's the correct end
            // state.
            if (savedManifest && savedManifest.length > 0) {
                await plugin.storage.setManifest(savedManifest);
            }
            plugin.storage.setLamport(savedLamport);
            await plugin.storage.deleteContent('legacy-node');

            return {
                manifestMigrated:
                    !!result.manifestInStorage &&
                    result.manifestInStorage.length === 5 &&
                    result.manifestInStorage[0] === 10 &&
                    result.manifestInStorage[4] === 50,
                lamportMigrated: result.lamportInStorage === 7,
                contentMigrated:
                    !!result.legacyContentInStorage &&
                    result.legacyContentInStorage.length === 3 &&
                    result.legacyContentInStorage[0] === 100,
                manifestPathRemoved: !result.manifestPathStillOnDisk,
                lamportPathRemoved: !result.lamportPathStillOnDisk,
                contentFileRemoved: !result.contentFileStillOnDisk,
                v1RootRemoved: !result.v1RootStillOnDisk,
            };
        });
        expect(phaseC.manifestMigrated).toBe(true);
        expect(phaseC.lamportMigrated).toBe(true);
        expect(phaseC.contentMigrated).toBe(true);
        expect(phaseC.manifestPathRemoved).toBe(true);
        expect(phaseC.lamportPathRemoved).toBe(true);
        expect(phaseC.contentFileRemoved).toBe(true);
        expect(phaseC.v1RootRemoved).toBe(true);
        console.log('[#94] phase C: on-disk state migrated to IndexedDB; on-disk artifacts removed');

        // --------------------------------------------------------------
        // Phase D — defense-in-depth: if a path under our own plugin
        //   folder ever reaches `ingestOrSyncHiddenFile`, the call
        //   refuses to upload and logs an error rather than looping.
        //
        //   Triggers normally don't reach this branch because the
        //   ignore-pattern logic in `scanHiddenFiles` filters our own
        //   folder out. We invoke directly to test the safety net.
        // --------------------------------------------------------------
        const phaseD: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            const cd = (app as any).vault.configDir;
            const ownPath = `${cd}/plugins/${plugin.manifest.id}/data.json`;

            // Capture console.error output during the call.
            const errors: string[] = [];
            const origError = console.error;
            console.error = (...args: unknown[]) => {
                errors.push(args.map((a) => String(a)).join(' '));
            };
            try {
                await plugin.ingestOrSyncHiddenFile(ownPath);
            } finally {
                console.error = origError;
            }

            const adapter = (app as any).vault.adapter;
            const manifestEntries = JSON.parse(
                plugin.client.projectionJson(),
            );
            return {
                hadOwnPathError: errors.some((e) => e.includes('own plugin folder')),
                ownPathInProjection: manifestEntries.some(
                    (r: any) => r.path === ownPath,
                ),
                ownPathOnDisk: await adapter.exists(ownPath), // settings exist
            };
        });
        expect(phaseD.hadOwnPathError).toBe(true);
        expect(phaseD.ownPathInProjection).toBe(false); // never uploaded
        expect(phaseD.ownPathOnDisk).toBe(true); // settings file untouched
        console.log('[#94] phase D: ingest of own plugin path refused + error logged');
    });
});
