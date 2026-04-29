// Regression test for #91 — self-write fs-event suppression cookie.
//
// When Syncline writes a file (vault.modify, vault.adapter.writeBinary,
// etc.), Obsidian's vault watcher fires an event for that path. The
// plugin must drop that echo, otherwise it re-reads, re-hashes, and
// can re-broadcast its own write. Before this PR the suppression was
// per-event-kind sets cleaned up via setTimeout — racy on slow machines
// and easy to leak. After this PR it's per-kind Maps with explicit
// expiry timestamps, lazy-swept on read and periodically full-swept.
//
// The cookie machinery itself doesn't require a server or vault state —
// it's purely state on the plugin object. This test pokes
// plugin.markSelfWrite / isSelfWriteEcho / sweepExpiredSelfWriteCookies
// directly via executeObsidian and asserts the contract.

import { expect, browser } from '@wdio/globals';

describe('Syncline #91 — self-write fs-event suppression cookie', () => {

    before(async function () {
        this.timeout(60_000);
        const obsidianPage = browser.getObsidianPage();
        try { await obsidianPage.enablePlugin('syncline'); } catch {}
        // The plugin doesn't need to be connected for the cookie
        // machinery; we exercise it as pure state on the plugin object.
        // But it does need to be loaded.
        await browser.waitUntil(async () => browser.executeObsidian(async ({ app }) => {
            return !!(app as any).plugins.plugins['syncline'];
        }), { timeout: 30_000, interval: 200 });
    });

    it('marks, expires, and sweeps self-write cookies', async function () {
        this.timeout(60_000);

        // --------------------------------------------------------------
        // Phase A — basic round-trip: mark, then read back.
        // --------------------------------------------------------------
        const phaseA: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            // Start clean.
            for (const m of Object.values(plugin.selfWriteCookies)) (m as Map<string, number>).clear();
            plugin.markSelfWrite('modify', 'foo.md');
            return {
                isEcho: plugin.isSelfWriteEcho('modify', 'foo.md'),
                mapSize: plugin.selfWriteCookies.modify.size,
            };
        });
        expect(phaseA.isEcho).toBe(true);
        expect(phaseA.mapSize).toBe(1);
        console.log('[#91] phase A: mark + read-back works');

        // --------------------------------------------------------------
        // Phase B — per-kind isolation: a `modify` cookie does NOT
        // suppress a `rename` of the same path. This is the property
        // the per-kind separation exists to preserve — losing it would
        // drop user-initiated renames that follow a self-write.
        // --------------------------------------------------------------
        const phaseB: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            for (const m of Object.values(plugin.selfWriteCookies)) (m as Map<string, number>).clear();
            plugin.markSelfWrite('modify', 'bar.md');
            return {
                modifyEcho: plugin.isSelfWriteEcho('modify', 'bar.md'),
                createEcho: plugin.isSelfWriteEcho('create', 'bar.md'),
                deleteEcho: plugin.isSelfWriteEcho('delete', 'bar.md'),
                renameEcho: plugin.isSelfWriteEcho('rename', 'bar.md'),
                anyEcho: plugin.isAnySelfWriteEcho('bar.md'),
            };
        });
        expect(phaseB.modifyEcho).toBe(true);
        expect(phaseB.createEcho).toBe(false);
        expect(phaseB.deleteEcho).toBe(false);
        expect(phaseB.renameEcho).toBe(false);
        expect(phaseB.anyEcho).toBe(true);
        console.log('[#91] phase B: per-kind isolation holds');

        // --------------------------------------------------------------
        // Phase C — expiry: an entry whose expiry timestamp is in the
        // past must read as not-echo AND be lazy-swept on read.
        // --------------------------------------------------------------
        const phaseC: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            for (const m of Object.values(plugin.selfWriteCookies)) (m as Map<string, number>).clear();
            // Backdate expiry by 1ms — simulates a cookie whose TTL has lapsed.
            plugin.selfWriteCookies.modify.set('stale.md', Date.now() - 1);
            const isEchoBefore = plugin.isSelfWriteEcho('modify', 'stale.md');
            const sizeAfter = plugin.selfWriteCookies.modify.size;
            return { isEchoBefore, sizeAfter };
        });
        expect(phaseC.isEchoBefore).toBe(false);
        expect(phaseC.sizeAfter).toBe(0); // lazy-swept
        console.log('[#91] phase C: expired cookies read as false and lazy-sweep');

        // --------------------------------------------------------------
        // Phase D — bounded growth: 1 000 expired entries, then a full
        // sweep, then verify the maps are empty. The lazy sweep only
        // fires on read; the periodic sweep is the safety net for
        // paths whose echo event never lands.
        // --------------------------------------------------------------
        const phaseD: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            for (const m of Object.values(plugin.selfWriteCookies)) (m as Map<string, number>).clear();
            const past = Date.now() - 1;
            for (let i = 0; i < 250; i++) {
                plugin.selfWriteCookies.modify.set(`m${i}.md`, past);
                plugin.selfWriteCookies.create.set(`c${i}.md`, past);
                plugin.selfWriteCookies.delete.set(`d${i}.md`, past);
                plugin.selfWriteCookies.rename.set(`r${i}.md`, past);
            }
            const before = (
                plugin.selfWriteCookies.modify.size +
                plugin.selfWriteCookies.create.size +
                plugin.selfWriteCookies.delete.size +
                plugin.selfWriteCookies.rename.size
            );
            plugin.sweepExpiredSelfWriteCookies();
            const after = (
                plugin.selfWriteCookies.modify.size +
                plugin.selfWriteCookies.create.size +
                plugin.selfWriteCookies.delete.size +
                plugin.selfWriteCookies.rename.size
            );
            return { before, after };
        });
        expect(phaseD.before).toBe(1000);
        expect(phaseD.after).toBe(0);
        console.log('[#91] phase D: full sweep clears all expired entries');

        // --------------------------------------------------------------
        // Phase E — sweep preserves unexpired entries. Bounding memory
        // mustn't drop active cookies whose corresponding fs event
        // hasn't fired yet.
        // --------------------------------------------------------------
        const phaseE: any = await browser.executeObsidian(async ({ app }) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            for (const m of Object.values(plugin.selfWriteCookies)) (m as Map<string, number>).clear();
            const past = Date.now() - 1;
            const future = Date.now() + 60_000;
            plugin.selfWriteCookies.modify.set('expired.md', past);
            plugin.selfWriteCookies.modify.set('active.md', future);
            plugin.sweepExpiredSelfWriteCookies();
            return {
                hasExpired: plugin.selfWriteCookies.modify.has('expired.md'),
                hasActive: plugin.selfWriteCookies.modify.has('active.md'),
                activeEcho: plugin.isSelfWriteEcho('modify', 'active.md'),
            };
        });
        expect(phaseE.hasExpired).toBe(false);
        expect(phaseE.hasActive).toBe(true);
        expect(phaseE.activeEcho).toBe(true);
        console.log('[#91] phase E: sweep preserves unexpired entries');
    });
});
