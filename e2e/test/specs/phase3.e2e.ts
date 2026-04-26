// Phase 3: Obsidian-side add convergence.
//
// 1. Start a fresh syncline server + CLI sync against an empty dir.
// 2. Launch Obsidian (fresh empty vault) with the plugin pointed at
//    the server. Wait for handshake — both sides empty so this
//    converges trivially.
// 3. Copy the source vault contents INTO the OBSIDIAN vault dir, batch
//    by batch (raw filesystem writes, the same way a user would drag
//    files into the vault folder via Finder). Obsidian's watcher
//    surfaces the disk events to the plugin, which uploads them to the
//    server. The CLI then picks them up via the manifest broadcast.
// 4. Assert source ≡ CLI ≡ Obsidian, modulo the tolerance set.

import { spawn, execSync, ChildProcess } from 'child_process';
import { join, relative } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline Phase 3 — Obsidian-side add', () => {
    const port = 3052;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'phase3.db');
    const cliFolderPath = join(e2eDir, 'phase3-cli-vault');
    const sourceVault = '/tmp/syncline-work/cache/extracted/obsidian-tom';
    const synclineBin = join(repoRoot, 'target/release/syncline');
    const TOLERATED_PATHS = new Set<string>([
        '05 🧪 Výzkum/tech/datasheets/hithium-cell-brochure.pdf', // #59
        '99 🗃️ Archiv/openclaw/scrape_final_v2.py', // #37 (0-byte binary)
        'scripts/kg-query.py', // #37 (0-byte binary)
        'scripts/podcast-state.json', // #37 (0-byte binary)
    ]);
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
    function diff(a: Map<string, string>, b: Map<string, string>) {
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
    async function waitFor<T>(label: string, fn: () => Promise<T | null | undefined | false | ''>, timeoutMs = 300_000, stepMs = 1000): Promise<T> {
        const deadline = Date.now() + timeoutMs;
        while (Date.now() < deadline) {
            try { const v = await fn(); if (v) return v as T; } catch {}
            await browser.pause(stepMs);
        }
        throw new Error(`waitFor("${label}") timed out after ${timeoutMs}ms`);
    }

    before(async function () {
        this.timeout(20 * 60_000);
        if (!fs.existsSync(synclineBin)) throw new Error(`syncline binary missing at ${synclineBin}`);
        if (!fs.existsSync(sourceVault)) {
            console.log(`[skip] source vault fixture not present at ${sourceVault}`);
            return this.skip();
        }
        if (fs.existsSync(dbPath)) fs.unlinkSync(dbPath);
        if (fs.existsSync(cliFolderPath)) fs.rmSync(cliFolderPath, { recursive: true, force: true });
        fs.mkdirSync(cliFolderPath, { recursive: true });

        let serverOut = '';
        serverProc = spawn(synclineBin, ['server', '--port', String(port), '--db-path', dbPath], { stdio: 'pipe' });
        serverProc.stdout?.on('data', (d) => { serverOut += d; process.stdout.write('SRV: ' + d); });
        serverProc.stderr?.on('data', (d) => { serverOut += d; process.stderr.write('SRV: ' + d); });
        await waitFor('server listening', async () => /listening/i.test(serverOut), 30_000);

        let cliOut = '';
        cliProc = spawn(synclineBin, ['sync', '-f', cliFolderPath, '-u', serverUrl, '--log-level', 'info'], { stdio: 'pipe' });
        cliProc.stdout?.on('data', (d) => { cliOut += d; process.stdout.write('CLI: ' + d); });
        cliProc.stderr?.on('data', (d) => { cliOut += d; process.stderr.write('CLI: ' + d); });
        await waitFor('CLI handshake', async () => /v1 handshake OK/.test(cliOut), 30_000);

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
        }), 30_000);
    });

    after(() => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        if (serverProc && !serverProc.killed) serverProc.kill();
    });

    it('Obsidian add: copy source vault into Obsidian dir, all three converge', async function () {
        this.timeout(45 * 60_000);

        const vaultPath: string = await browser.executeObsidian(async ({ app }) => (app as any).vault.adapter.basePath as string);
        console.log('Obsidian vault path:', vaultPath);

        // Batch the copy in folder size order — same shape as phase 1 / 2.
        const dirs = fs.readdirSync(sourceVault, { withFileTypes: true })
            .filter((e) => e.isDirectory() && e.name !== '.syncline')
            .map((e) => e.name)
            .sort((a, b) => parseInt(execSync(`du -sk "${join(sourceVault, a)}"`).toString().split('\t')[0], 10)
                          - parseInt(execSync(`du -sk "${join(sourceVault, b)}"`).toString().split('\t')[0], 10));

        for (const d of dirs) {
            const src = join(sourceVault, d) + '/';
            const dst = join(vaultPath, d) + '/';
            fs.mkdirSync(dst, { recursive: true });
            const excludesForBatch = [...TOLERATED_PATHS]
                .filter((p) => p.startsWith(d + '/'))
                .map((p) => p.slice(d.length + 1))
                .map((p) => `--exclude=${JSON.stringify(p)}`)
                .join(' ');
            execSync(`rsync -a ${excludesForBatch} "${src}" "${dst}"`);
            console.log(`copied: ${d}`);
            // Plugin's bulk file-create handler (#58) is more sensitive
            // than CLI's debounced scanner. A wider per-batch pause lets
            // it drain its reconcile queue before the next batch lands.
            await browser.pause(8000);
        }
        for (const ent of fs.readdirSync(sourceVault, { withFileTypes: true })) {
            if (!ent.isFile()) continue;
            if (TOLERATED_PATHS.has(ent.name)) continue;
            execSync(`cp "${join(sourceVault, ent.name)}" "${vaultPath}/"`);
        }
        await browser.pause(10000);

        const expectedSize = listVault(sourceVault).size;
        const expectedNonEmpty = [...listVault(sourceVault).values()].filter((s) => s !== EMPTY_SHA).length;

        // Settle on the CLI side this time — the data flows
        // Obsidian → server → CLI, so CLI is the lagging one.
        let lastNonEmpty = -1, lastBin = -1, stableTicks = 0;
        await waitFor('all sides settle', async () => {
            const cli = listVault(cliFolderPath);
            const nonEmpty = [...cli.values()].filter((s) => s !== EMPTY_SHA).length;
            const bin = [...cli.keys()].filter((k) => !k.endsWith('.md') && !k.endsWith('.txt')).length;
            if (nonEmpty === lastNonEmpty && bin === lastBin) stableTicks++; else stableTicks = 0;
            lastNonEmpty = nonEmpty; lastBin = bin;
            const st: any = await browser.executeObsidian(async ({ app }) => {
                const p: any = (app as any).plugins.plugins['syncline'];
                return p ? { proj: p.lastProjection?.size ?? 0, subs: p.subscribedContent?.size ?? 0, blobs: p.requestedBlobs?.size ?? 0, conn: p.client?.isConnected?.() ?? false } : null;
            });
            console.log(`[settle] cli_nonEmpty=${nonEmpty}/${expectedNonEmpty} cli_bin=${bin}/161 proj=${st?.proj} subs=${st?.subs} blobs=${st?.blobs} conn=${st?.conn} stable=${stableTicks}`);
            return stableTicks >= 12;
        }, 45 * 60_000, 5000);

        const archive = '/tmp/syncline-work/cache/phase3-obsidian-vault';
        if (fs.existsSync(archive)) fs.rmSync(archive, { recursive: true, force: true });
        execSync(`cp -a "${vaultPath}" "${archive}"`);

        const src = listVault(sourceVault);
        const cli = listVault(cliFolderPath);
        const obs = listVault(vaultPath);
        const dSrcCli = diff(src, cli);
        const dSrcObs = diff(src, obs);
        const dCliObs = diff(cli, obs);
        console.log('source vs CLI:', JSON.stringify(dSrcCli, null, 2));
        console.log('source vs Obsidian:', JSON.stringify(dSrcObs, null, 2));
        console.log('CLI vs Obsidian:', JSON.stringify(dCliObs, null, 2));
        expect(dSrcCli.only_in_a).toHaveLength(0);
        expect(dSrcCli.only_in_b).toHaveLength(0);
        expect(dSrcCli.sha_differs).toHaveLength(0);
        expect(dSrcObs.only_in_a).toHaveLength(0);
        expect(dSrcObs.only_in_b).toHaveLength(0);
        expect(dSrcObs.sha_differs).toHaveLength(0);
        expect(dCliObs.only_in_a).toHaveLength(0);
        expect(dCliObs.only_in_b).toHaveLength(0);
        expect(dCliObs.sha_differs).toHaveLength(0);
    });
});
