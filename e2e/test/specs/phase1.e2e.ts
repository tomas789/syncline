// Phase 1: Restore-then-launch convergence.
//
// 1. Start a fresh syncline server + CLI sync against an empty dir.
// 2. rsync the cached 13:47 vault tarball INTO that dir, batch by batch.
// 3. Wait for CLI ↔ server convergence.
// 4. Launch Obsidian (fresh vault, plugin pre-installed) and point the
//    plugin at the server.
// 5. Wait for plugin ↔ server convergence (plugin pulls the manifest +
//    every content subdoc + every blob ≤ 16 MiB).
// 6. Assert all three (source / CLI dir / Obsidian vault) are byte-
//    identical, modulo:
//      - the 25 MB hithium pdf (#59 — over the WS frame limit)
//      - lowercase `archive/` vs capital `Archive/` collision (#56 — both
//        get materialised at the case-folded location on macOS APFS)

import { spawn, execSync, ChildProcess } from 'child_process';
import { join, dirname, relative } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline Phase 1 — restore-then-launch', () => {
    const port = 3050;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'phase1.db');
    const cliFolderPath = join(e2eDir, 'phase1-cli-vault');
    const sourceVault = '/tmp/syncline-work/cache/extracted/obsidian-tom';
    const synclineBin = join(repoRoot, 'target/release/syncline');

    // Source-vault paths we tolerate diverging from the strict
    // byte-equality assertion. All have open issues.
    const TOLERATED_PATHS = new Set<string>([
        '05 🧪 Výzkum/tech/datasheets/hithium-cell-brochure.pdf', // #59
        '99 🗃️ Archiv/openclaw/scrape_final_v2.py', // #37 (0-byte binary)
        'scripts/kg-query.py', // #37 (0-byte binary)
        'scripts/podcast-state.json', // #37 (0-byte binary)
    ]);

    let serverProc: ChildProcess;
    let cliProc: ChildProcess;

    function fileSha(p: string): string {
        const h = crypto.createHash('sha256');
        h.update(fs.readFileSync(p));
        return h.digest('hex');
    }

    function listVault(root: string): Map<string, string> {
        // Returns rel-path → sha256 for every regular file under root,
        // skipping the .syncline/ state dir, Obsidian's .obsidian/,
        // transient `.tmp` files atomic_write briefly creates between
        // its `File::create` and `fs::rename` steps, and dotfiles
        // (e.g. the `.gitkeep` shipped in `e2e/test-vault/`, which
        // syncline doesn't track but lives at the root of the fresh
        // Obsidian vault wdio-obsidian-service spawns).
        const out = new Map<string, string>();
        function walk(dir: string) {
            for (const ent of fs.readdirSync(dir, { withFileTypes: true })) {
                if (ent.name.startsWith('.')) continue;
                if (ent.name.endsWith('.tmp')) continue;
                const abs = join(dir, ent.name);
                const rel = relative(root, abs);
                if (ent.isDirectory()) walk(abs);
                else if (ent.isFile()) out.set(rel, fileSha(abs));
            }
        }
        walk(root);
        return out;
    }

    function diff(a: Map<string, string>, b: Map<string, string>): {
        only_in_a: string[];
        only_in_b: string[];
        sha_differs: string[];
    } {
        const only_in_a: string[] = [];
        const only_in_b: string[] = [];
        const sha_differs: string[] = [];
        for (const [k, v] of a) {
            if (TOLERATED_PATHS.has(k)) continue;
            if (!b.has(k)) only_in_a.push(k);
            else if (b.get(k) !== v) sha_differs.push(k);
        }
        for (const k of b.keys()) {
            if (TOLERATED_PATHS.has(k)) continue;
            if (!a.has(k)) only_in_b.push(k);
        }
        return { only_in_a, only_in_b, sha_differs };
    }

    async function waitFor<T>(
        label: string,
        fn: () => Promise<T | null | undefined | false | ''>,
        timeoutMs = 300_000,
        stepMs = 1000,
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
            `waitFor("${label}") timed out after ${timeoutMs}ms` +
            (lastErr ? `: ${String(lastErr)}` : ''),
        );
    }

    before(async function () {
        this.timeout(20 * 60_000);

        if (!fs.existsSync(synclineBin)) {
            throw new Error(`syncline binary missing at ${synclineBin}; build with cargo build --release`);
        }
        if (!fs.existsSync(sourceVault)) {
            console.log(`[skip] source vault fixture not present at ${sourceVault}`);
            return this.skip();
        }

        // Clean any leftover state.
        if (fs.existsSync(dbPath)) fs.unlinkSync(dbPath);
        if (fs.existsSync(cliFolderPath)) fs.rmSync(cliFolderPath, { recursive: true, force: true });
        fs.mkdirSync(cliFolderPath, { recursive: true });

        // ---- Server ----
        let serverOut = '';
        serverProc = spawn(synclineBin, ['server', '--port', String(port), '--db-path', dbPath], {
            stdio: 'pipe',
        });
        serverProc.stdout?.on('data', (d) => { serverOut += d; process.stdout.write('SRV: ' + d); });
        serverProc.stderr?.on('data', (d) => { serverOut += d; process.stderr.write('SRV: ' + d); });
        await waitFor('server listening', async () => /listening/i.test(serverOut), 30_000);

        // ---- CLI sync ----
        let cliOut = '';
        cliProc = spawn(synclineBin, ['sync', '-f', cliFolderPath, '-u', serverUrl, '--log-level', 'info'], {
            stdio: 'pipe',
        });
        cliProc.stdout?.on('data', (d) => { cliOut += d; process.stdout.write('CLI: ' + d); });
        cliProc.stderr?.on('data', (d) => { cliOut += d; process.stderr.write('CLI: ' + d); });
        await waitFor('CLI handshake', async () => /v1 handshake OK/.test(cliOut), 30_000);
    });

    after(() => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        if (serverProc && !serverProc.killed) serverProc.kill();
    });

    it('rsync vault → CLI dir, server converges', async function () {
        this.timeout(10 * 60_000);

        // Top-level dirs in size-ascending order; the bulk-scan reconnect
        // explosion (#60) makes a single rsync risky on cold cargo builds.
        const dirs = fs
            .readdirSync(sourceVault, { withFileTypes: true })
            .filter((e) => e.isDirectory() && e.name !== '.syncline')
            .map((e) => e.name)
            .sort((a, b) => {
                const sizeA = execSync(`du -sk "${join(sourceVault, a)}"`).toString().split('\t')[0];
                const sizeB = execSync(`du -sk "${join(sourceVault, b)}"`).toString().split('\t')[0];
                return parseInt(sizeA, 10) - parseInt(sizeB, 10);
            });

        // rsync, excluding TOLERATED_PATHS (e.g. the 25 MB hithium pdf
        // which would trip the 16 MiB WS frame limit (#59) and reset
        // the connection mid-loop, dropping every blob queued after
        // it). rsync excludes are evaluated relative to each batch
        // source dir, not the vault root, so we strip the leading
        // top-level component when it matches the current batch and
        // skip the rest.
        for (const d of dirs) {
            const src = join(sourceVault, d) + '/';
            const dst = join(cliFolderPath, d) + '/';
            fs.mkdirSync(dst, { recursive: true });
            const excludesForBatch = [...TOLERATED_PATHS]
                .filter((p) => p.startsWith(d + '/'))
                .map((p) => p.slice(d.length + 1))
                .map((p) => `--exclude=${JSON.stringify(p)}`)
                .join(' ');
            execSync(`rsync -a ${excludesForBatch} "${src}" "${dst}"`);
            console.log(`copied: ${d}`);
            await browser.pause(5000);
        }

        // Loose top-level files.
        for (const ent of fs.readdirSync(sourceVault, { withFileTypes: true })) {
            if (!ent.isFile()) continue;
            if (TOLERATED_PATHS.has(ent.name)) continue;
            execSync(`cp "${join(sourceVault, ent.name)}" "${cliFolderPath}/"`);
        }
        await browser.pause(8000);

        // Wait for CLI ↔ server. Crude: wait for the source path-set ⊆
        // CLI path-set, then a quiet period.
        const expectedPaths = new Set<string>([...listVault(sourceVault).keys()]);
        await waitFor('CLI vault has all expected paths', async () => {
            const have = listVault(cliFolderPath);
            for (const p of expectedPaths) {
                if (!TOLERATED_PATHS.has(p) && !have.has(p)) return false;
            }
            return true;
        }, 5 * 60_000, 2000);

        // Now compare contents.
        const src = listVault(sourceVault);
        const cli = listVault(cliFolderPath);
        const d = diff(src, cli);
        console.log('source vs CLI:', JSON.stringify(d, null, 2));
        expect(d.only_in_a).toHaveLength(0);
        expect(d.only_in_b).toHaveLength(0);
        expect(d.sha_differs).toHaveLength(0);
    });

    it('Obsidian plugin pulls full manifest + content from server', async function () {
        this.timeout(15 * 60_000);

        const obsidianPage = browser.getObsidianPage();
        try {
            await obsidianPage.enablePlugin('syncline-obsidian');
        } catch (e: any) {
            console.log('plugin enable:', e?.message);
        }

        await browser.executeObsidian(async ({ app }, url) => {
            const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
            if (!plugin) throw new Error('Syncline plugin not found in app.plugins');
            plugin.settings.serverUrl = url;
            await plugin.saveSettings();
            plugin.disconnect();
            await plugin.connect();
        }, serverUrl);

        // Get the vault path that wdio-obsidian-service used.
        const vaultPath: string = await browser.executeObsidian(async ({ app }) => {
            return (app as any).vault.adapter.basePath as string;
        });
        console.log('Obsidian vault path:', vaultPath);

        // Wait until Obsidian's projection size matches the CLI's. The
        // plugin's status bar reads `lastProjection.size`; we sample it.
        const expectedSize = listVault(sourceVault).size; // 1305
        await waitFor('plugin projection settles', async () => {
            const size: number = await browser.executeObsidian(async ({ app }) => {
                const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
                return plugin && plugin.lastProjection ? plugin.lastProjection.size : 0;
            });
            console.log(`plugin lastProjection.size = ${size} (expecting ~${expectedSize})`);
            return size >= expectedSize - TOLERATED_PATHS.size;
        }, 10 * 60_000, 5000);

        // Settle period: wait for the count of TEXT and BINARY files to
        // stop growing INDEPENDENTLY. Counting only "non-empty files" is
        // a trap because most binary files arriving via blob-request are
        // non-empty regardless, but text placeholders that never get
        // their content also count as "0 bytes" and never tick the
        // counter — so without the binary-specific check we'd settle
        // before any blob actually downloaded.
        const EMPTY_SHA = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';
        let lastNonEmpty = -1;
        let lastBinary = -1;
        let stableTicks = 0;
        const expectedNonEmpty = [...listVault(sourceVault).values()].filter(
            (sha) => sha !== EMPTY_SHA,
        ).length;
        await waitFor('plugin disk content settles', async () => {
            const v = listVault(vaultPath);
            const nonEmpty = [...v.values()].filter((sha) => sha !== EMPTY_SHA).length;
            const binary = [...v.keys()].filter(
                (k) => !k.endsWith('.md') && !k.endsWith('.txt'),
            ).length;
            if (nonEmpty === lastNonEmpty && binary === lastBinary) stableTicks++;
            else stableTicks = 0;
            lastNonEmpty = nonEmpty;
            lastBinary = binary;

            const pluginState: any = await browser.executeObsidian(async ({ app }) => {
                const plugin: any = (app as any).plugins.plugins['syncline-obsidian'];
                if (!plugin) return null;
                return {
                    projection: plugin.lastProjection?.size ?? 0,
                    subscribedContent: plugin.subscribedContent?.size ?? 0,
                    requestedBlobs: plugin.requestedBlobs?.size ?? 0,
                    isConnected: plugin.client?.isConnected?.() ?? false,
                };
            });
            console.log(
                `[settle] nonEmpty=${nonEmpty}/${expectedNonEmpty} bin=${binary}/161 ` +
                `proj=${pluginState?.projection ?? '?'} subs=${pluginState?.subscribedContent ?? '?'} ` +
                `blobs=${pluginState?.requestedBlobs ?? '?'} conn=${pluginState?.isConnected ?? '?'} ` +
                `stable=${stableTicks}`,
            );
            // 60-second quiet window — big binaries take a while.
            return stableTicks >= 12;
        }, 30 * 60_000, 5000);

        // Save the final vault state for inspection if the assertion fails.
        const archive = '/tmp/syncline-work/cache/phase1-obsidian-vault';
        if (fs.existsSync(archive)) fs.rmSync(archive, { recursive: true, force: true });
        execSync(`cp -a "${vaultPath}" "${archive}"`);
        console.log(`saved obsidian vault → ${archive}`);

        const src = listVault(sourceVault);
        const obs = listVault(vaultPath);
        const d = diff(src, obs);
        console.log('source vs Obsidian:', JSON.stringify({
            only_in_source_count: d.only_in_a.length,
            only_in_obsidian_count: d.only_in_b.length,
            sha_differs_count: d.sha_differs.length,
            only_in_source_sample: d.only_in_a.slice(0, 5),
            only_in_obsidian_sample: d.only_in_b.slice(0, 5),
            sha_differs_sample: d.sha_differs.slice(0, 5),
        }, null, 2));
        expect(d.only_in_a).toHaveLength(0);
        expect(d.only_in_b).toHaveLength(0);
        expect(d.sha_differs).toHaveLength(0);
    });

    it('CLI dir == Obsidian vault (cross-check)', async function () {
        this.timeout(60_000);
        const vaultPath: string = await browser.executeObsidian(async ({ app }) => {
            return (app as any).vault.adapter.basePath as string;
        });
        const cli = listVault(cliFolderPath);
        const obs = listVault(vaultPath);
        const d = diff(cli, obs);
        console.log('CLI vs Obsidian:', JSON.stringify({
            only_in_cli_count: d.only_in_a.length,
            only_in_obsidian_count: d.only_in_b.length,
            sha_differs_count: d.sha_differs.length,
        }, null, 2));
        expect(d.only_in_a).toHaveLength(0);
        expect(d.only_in_b).toHaveLength(0);
        expect(d.sha_differs).toHaveLength(0);
    });
});
