// Regression test for #63 — ingestNewFile / reconcile race that
// drops content for files whose manifest-observer-triggered reconcile
// reaches the row before the subscribe IIFE adds nodeId to
// `subscribedContent`.
//
// Repro shape:
//   1. Start fresh server + CLI sync against an empty dir.
//   2. Launch Obsidian (empty vault) with the plugin pointed at the
//      server.
//   3. Drop N text files of varied size into the Obsidian vault dir
//      via raw filesystem writes (NOT via the Obsidian API). Each
//      file has unique content keyed by index so we can detect any
//      empty / wrong-content file by comparison.
//   4. Wait for sync to settle.
//   5. Assert EVERY file's content survived on the Obsidian side
//      with byte-equality. No tolerance — the race must be 0%.
//
// The race rate in phase3 was ~0.23% (3 / 1300 files). Each run
// here is N files; for N=200 we'd expect ~0.5 dropped files per run
// with the bug present. We run the same scenario REPEATS times
// inside one mocha test so the cumulative trial size is large enough
// to fail reliably if the race is back. Each iteration uses a fresh
// vault path, fresh CLI dir, fresh server DB.

import { spawn, execSync, ChildProcess } from 'child_process';
import { join, relative } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline #63 — ingestNewFile race regression', () => {
    const port = 3055;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'issue63.db');
    const cliFolderPath = join(e2eDir, 'issue63-cli-vault');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    const N_FILES = 200;
    const REPEATS = 3;

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
    async function waitFor<T>(label: string, fn: () => Promise<T | null | undefined | false | ''>, timeoutMs = 60_000, stepMs = 500): Promise<T> {
        const deadline = Date.now() + timeoutMs;
        while (Date.now() < deadline) {
            try { const v = await fn(); if (v) return v as T; } catch {}
            await browser.pause(stepMs);
        }
        throw new Error(`waitFor("${label}") timed out after ${timeoutMs}ms`);
    }

    // Generate a corpus of N unique-content text files. Sizes vary
    // from a few bytes to ~10 KiB to surface size-dependent races.
    function makeCorpus(): Map<string, string> {
        const files = new Map<string, string>();
        for (let i = 0; i < N_FILES; i++) {
            const sizeBytes = 50 + ((i * 137) % 10_000);
            const seed = crypto.createHash('sha256').update(`#63-file-${i}`).digest('hex');
            // Repeat the seed to fill the desired size; keeps content
            // deterministic and unique per index.
            let content = `# file ${i}\n\n`;
            while (content.length < sizeBytes) content += seed + '\n';
            files.set(`note-${String(i).padStart(4, '0')}.md`, content);
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
        serverProc.stdout?.on('data', (d) => { serverOut += d; process.stdout.write('SRV: ' + d); });
        serverProc.stderr?.on('data', (d) => { serverOut += d; process.stderr.write('SRV: ' + d); });
        await waitFor('server listening', async () => /listening/i.test(serverOut), 30_000);

        let cliOut = '';
        cliProc = spawn(synclineBin, ['sync', '-f', cliFolderPath, '-u', serverUrl, '--log-level', 'info'], { stdio: 'pipe' });
        cliProc.stdout?.on('data', (d) => { cliOut += d; process.stdout.write('CLI: ' + d); });
        cliProc.stderr?.on('data', (d) => { cliOut += d; process.stderr.write('CLI: ' + d); });
        await waitFor('CLI handshake', async () => /v1 handshake OK/.test(cliOut), 30_000);

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
        }), 30_000);
    });

    after(() => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        if (serverProc && !serverProc.killed) serverProc.kill();
    });

    it(`drops ${N_FILES} files into Obsidian vault, ${REPEATS}× — every file's content survives`, async function () {
        this.timeout(15 * 60_000);

        const vaultPath: string = await browser.executeObsidian(async ({ app }) => (app as any).vault.adapter.basePath as string);

        const corpus = makeCorpus();
        // Expected hash per filename — used after settle to find any
        // file whose Obsidian-side bytes don't match the source.
        const expected = new Map<string, string>();
        for (const [name, content] of corpus) {
            expected.set(name, crypto.createHash('sha256').update(content).digest('hex'));
        }

        let totalRacedFiles = 0;
        const racedExamples: string[] = [];

        for (let trial = 0; trial < REPEATS; trial++) {
            const trialDir = `trial-${trial}`;
            const trialPath = join(vaultPath, trialDir);
            fs.mkdirSync(trialPath, { recursive: true });

            // Drop all N files in tight succession via raw fs writes.
            // This simulates rsync / drag-and-drop behavior, where
            // Obsidian's watcher fires onFileCreate events in rapid
            // succession.
            for (const [name, content] of corpus) {
                fs.writeFileSync(join(trialPath, name), content);
            }
            console.log(`[#63 trial ${trial}] dropped ${corpus.size} files`);

            // Wait until on-disk file count for this trial dir stops
            // changing — that's the proxy for "plugin's reconcile
            // pipeline drained" without depending on plugin internals.
            let lastCount = -1;
            let stableTicks = 0;
            await waitFor(`trial ${trial} settle`, async () => {
                const present = fs.existsSync(trialPath)
                    ? fs.readdirSync(trialPath).filter((n) => n.endsWith('.md')).length
                    : 0;
                if (present === lastCount) stableTicks++; else stableTicks = 0;
                lastCount = present;
                if (stableTicks % 4 === 0) console.log(`[#63 trial ${trial}] settle present=${present}/${N_FILES} stable=${stableTicks}`);
                return stableTicks >= 10;
            }, 5 * 60_000, 1000);

            // Now compare every file's content. Any mismatch is a #63
            // race (file ended up empty / wrong on Obsidian).
            const trialMismatches: string[] = [];
            for (const [name, expectedHash] of expected) {
                const fpath = join(trialPath, name);
                if (!fs.existsSync(fpath)) {
                    trialMismatches.push(`${trialDir}/${name} [MISSING]`);
                    continue;
                }
                const actual = fileSha(fpath);
                if (actual !== expectedHash) {
                    const sz = fs.statSync(fpath).size;
                    trialMismatches.push(`${trialDir}/${name} [size=${sz}]`);
                }
            }
            console.log(`[#63 trial ${trial}] mismatches: ${trialMismatches.length} / ${N_FILES}`);
            if (trialMismatches.length > 0) {
                totalRacedFiles += trialMismatches.length;
                racedExamples.push(...trialMismatches.slice(0, 10));
            }
        }

        console.log(`[#63] total raced files across ${REPEATS} trials of ${N_FILES}: ${totalRacedFiles}`);
        if (totalRacedFiles > 0) {
            console.error(`[#63] sample raced files:\n${racedExamples.slice(0, 20).join('\n')}`);
        }
        expect(totalRacedFiles).toBe(0);
    });
});
