// Phase 4: claw.krej.ci-as-server convergence (cross-host).
//
// Same shape as phase 1 (rsync source vault into the local CLI dir,
// then watch Obsidian converge), but the server runs on the remote
// host claw.krej.ci instead of localhost. Validates that all wire
// formats survive an actual TCP/internet round-trip and that no
// localhost-only assumption (loopback shortcut, MTU, MSS, framing
// quirk) sneaks into the protocol.
//
// Pre-reqs:
//   - SSH access to claw.krej.ci (uses the user's agent).
//   - A pre-built linux x86_64 syncline binary at
//     ~/syncline-phase4/target/release/syncline on claw (see README).
//   - Inbound TCP REMOTE_PORT open from the test machine to claw.
//
// The remote server is started by a fresh ssh session at `before()` and
// killed by another ssh in `after()`. We pin a fixed port so the kill
// in teardown can find the right pid even if ssh disconnects mid-test.

import { spawn, execSync, ChildProcess } from 'child_process';
import { join, relative } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline Phase 4 — claw.krej.ci-as-server', () => {
    const REMOTE_HOST = 'claw.krej.ci';
    const REMOTE_PORT = 3060;
    const REMOTE_DB = '/tmp/syncline-phase4.db';
    const REMOTE_BIN = '~/syncline-phase4/target/release/syncline';
    const serverUrl = `ws://${REMOTE_HOST}:${REMOTE_PORT}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const cliFolderPath = join(e2eDir, 'phase4-cli-vault');
    const sourceVault = '/tmp/syncline-work/cache/extracted/obsidian-tom';
    const synclineBin = join(repoRoot, 'target/release/syncline');
    const TOLERATED_PATHS = new Set<string>([
        '05 🧪 Výzkum/tech/datasheets/hithium-cell-brochure.pdf', // #59
        '99 🗃️ Archiv/openclaw/scrape_final_v2.py', // #37
        'scripts/kg-query.py', // #37
        'scripts/podcast-state.json', // #37
    ]);
    const EMPTY_SHA = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';

    const REMOTE_LOG = '/tmp/phase4-server.log';
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
        if (fs.existsSync(cliFolderPath)) fs.rmSync(cliFolderPath, { recursive: true, force: true });
        fs.mkdirSync(cliFolderPath, { recursive: true });

        // Reachability check — skip the spec on environments without
        // SSH access to the remote (e.g. CI). On a developer machine
        // with a configured ssh-agent + ~/.ssh/config entry for
        // claw.krej.ci, the connection is authenticated automatically.
        try {
            execSync(`ssh -o ConnectTimeout=10 -o BatchMode=yes ${REMOTE_HOST} 'true'`, { stdio: 'pipe' });
        } catch {
            console.log(`[skip] cannot SSH to ${REMOTE_HOST} — phase4 needs a passwordless ssh-agent session to the remote`);
            return this.skip();
        }
        try {
            execSync(`ssh ${REMOTE_HOST} 'test -x ${REMOTE_BIN}'`, { stdio: 'pipe' });
        } catch {
            console.log(`[skip] remote binary missing at ${REMOTE_HOST}:${REMOTE_BIN} — build it first`);
            return this.skip();
        }

        // Stop any lingering phase4 servers and wipe the DB + log so we
        // poll a fresh "listening" line.
        try {
            execSync(`ssh ${REMOTE_HOST} "pkill -f 'syncline-phase4.*server' || true; rm -f ${REMOTE_DB} ${REMOTE_LOG}"`, { stdio: 'pipe' });
        } catch {}

        // Spawn the remote server detached (setsid -f) — ssh's pty
        // approach is flaky here, the shell exits before nohup can
        // ignore SIGHUP. setsid -f gives us a brand-new session and
        // returns immediately.
        execSync(
            `ssh ${REMOTE_HOST} "setsid -f ${REMOTE_BIN} server --port ${REMOTE_PORT} --db-path ${REMOTE_DB} > ${REMOTE_LOG} 2>&1 < /dev/null"`,
            { stdio: 'pipe' },
        );
        // Poll the remote log for the listening banner.
        await waitFor('remote server listening', async () => {
            try {
                const out = execSync(`ssh ${REMOTE_HOST} "cat ${REMOTE_LOG} 2>/dev/null"`, { stdio: 'pipe' }).toString();
                return /listening/i.test(out);
            } catch { return false; }
        }, 60_000, 1000);

        let cliOut = '';
        cliProc = spawn(synclineBin, ['sync', '-f', cliFolderPath, '-u', serverUrl, '--log-level', 'info'], { stdio: 'pipe' });
        cliProc.stdout?.on('data', (d) => { cliOut += d; process.stdout.write('CLI: ' + d); });
        cliProc.stderr?.on('data', (d) => { cliOut += d; process.stderr.write('CLI: ' + d); });
        await waitFor('CLI handshake', async () => /v1 handshake OK/.test(cliOut), 30_000);
    });

    after(async () => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        // Server is detached on the remote, so we kill explicitly.
        try {
            execSync(`ssh ${REMOTE_HOST} "pkill -f 'syncline-phase4.*server' || true"`, { stdio: 'pipe' });
        } catch {}
    });

    it('rsync vault → CLI dir, remote server propagates to Obsidian', async function () {
        this.timeout(45 * 60_000);

        // Copy in size-ascending order; same shape as phase 1.
        const dirs = fs.readdirSync(sourceVault, { withFileTypes: true })
            .filter((e) => e.isDirectory() && e.name !== '.syncline')
            .map((e) => e.name)
            .sort((a, b) => parseInt(execSync(`du -sk "${join(sourceVault, a)}"`).toString().split('\t')[0], 10)
                          - parseInt(execSync(`du -sk "${join(sourceVault, b)}"`).toString().split('\t')[0], 10));
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
        for (const ent of fs.readdirSync(sourceVault, { withFileTypes: true })) {
            if (!ent.isFile()) continue;
            if (TOLERATED_PATHS.has(ent.name)) continue;
            execSync(`cp "${join(sourceVault, ent.name)}" "${cliFolderPath}/"`);
        }
        await browser.pause(8000);

        // Wait for CLI ↔ remote server: source path-set ⊆ CLI path-set
        // (rsync alone makes this true the moment cp finishes — the
        // CLI scanner debounces but the bytes are on disk locally).
        // Then a quiet period for upload to drain.
        const expectedPaths = new Set<string>([...listVault(sourceVault).keys()]);
        await waitFor('CLI vault has all expected paths', async () => {
            const have = listVault(cliFolderPath);
            for (const p of expectedPaths) {
                if (!TOLERATED_PATHS.has(p) && !have.has(p)) return false;
            }
            return true;
        }, 5 * 60_000, 2000);

        // Now connect Obsidian and let it pull from the remote server.
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
        }), 60_000);
        const vaultPath: string = await browser.executeObsidian(async ({ app }) => (app as any).vault.adapter.basePath as string);

        const expectedNonEmpty = [...listVault(sourceVault).values()].filter((s) => s !== EMPTY_SHA).length;

        let lastNonEmpty = -1, lastBin = -1, stableTicks = 0;
        await waitFor('all sides settle', async () => {
            const obs = listVault(vaultPath);
            const nonEmpty = [...obs.values()].filter((s) => s !== EMPTY_SHA).length;
            const bin = [...obs.keys()].filter((k) => !k.endsWith('.md') && !k.endsWith('.txt')).length;
            if (nonEmpty === lastNonEmpty && bin === lastBin) stableTicks++; else stableTicks = 0;
            lastNonEmpty = nonEmpty; lastBin = bin;
            const st: any = await browser.executeObsidian(async ({ app }) => {
                const p: any = (app as any).plugins.plugins['syncline-obsidian'];
                return p ? { proj: p.lastProjection?.size ?? 0, subs: p.subscribedContent?.size ?? 0, blobs: p.requestedBlobs?.size ?? 0, conn: p.client?.isConnected?.() ?? false } : null;
            });
            console.log(`[settle] nonEmpty=${nonEmpty}/${expectedNonEmpty} bin=${bin}/161 proj=${st?.proj} subs=${st?.subs} blobs=${st?.blobs} conn=${st?.conn} stable=${stableTicks}`);
            // Higher quiet threshold than phase 1 — WAN RTT extends every
            // round-trip and a transient stall isn't a signal of done.
            return stableTicks >= 18;
        }, 45 * 60_000, 5000);

        const archive = '/tmp/syncline-work/cache/phase4-obsidian-vault';
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
