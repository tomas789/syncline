// Pre-existing-vault onboarding (Obsidian).
//
// User flow under test (mirrors a real first install):
//   1. `syncline server` runs.
//   2. A CLI peer is already attached to the server with an empty folder
//      (the user's "always on" backup peer).
//   3. The user opens Obsidian on a vault that ALREADY contains notes
//      and a binary attachment - all pre-existing on disk before the
//      plugin is ever enabled.
//   4. They install + enable the Syncline plugin and point it at the
//      server.
//   5. The plugin's first scan must push every pre-existing note + the
//      binary to the server.
//   6. The CLI peer must materialise everything byte-identical.
//   7. `.obsidian/workspace.json` is device-local and must NEVER
//      propagate, even though the plugin scans .obsidian/ for the
//      coreConfig category.
//   8. Bidirectional sync continues to work after onboarding.

import { spawn, ChildProcess } from 'child_process';
import { join, dirname } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline - Obsidian vault with pre-existing content joins server + CLI peer', () => {
    const port = 3070;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'pre-existing-vault.db');
    const cliFolderPath = join(e2eDir, 'pre-existing-vault-cli');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    // The corpus the user is bringing to Syncline. We focus on the
    // user-content surface (notes + attachments) and assert one
    // negative case for `.obsidian/workspace.json` (device-local).
    //
    // We do NOT seed any other `.obsidian/*` files because the
    // plugin's per-category configSync defaults are non-trivial
    // (DEFAULT_CONFIG_SYNC in main.ts: themes:true, snippets:true,
    // hotkeys:true, coreConfig:true, communityPluginList:false,
    // other:false). Testing those paths belongs in its own spec.
    type Item = { rel: string; bytes: Buffer };
    const userContent: Item[] = [
        { rel: 'welcome.md', bytes: Buffer.from('hello from pre-existing vault\n') },
        {
            rel: 'notes/daily/day1.md',
            bytes: Buffer.from('# Day 1\nfirst entry\n'),
        },
        {
            rel: 'assets/photo.jpg',
            bytes: Buffer.concat([
                Buffer.from([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]),
                Buffer.from('JFIF pretend-jpeg-bytes-with-some-noise'),
                Buffer.from([0xde, 0xad, 0xbe, 0xef]),
            ]),
        },
    ];

    // Device-local Obsidian state. Must never propagate.
    const deviceLocal: Item = {
        rel: '.obsidian/workspace.json',
        bytes: Buffer.from('{"layout":"device-local-private"}'),
    };

    let serverProc: ChildProcess;
    let cliProc: ChildProcess;

    function fileSha(p: string): string {
        return crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
    }
    function expectedSha(item: Item): string {
        return crypto.createHash('sha256').update(item.bytes).digest('hex');
    }

    async function waitFor<T>(
        label: string,
        fn: () => Promise<T | null | undefined | false | ''>,
        timeoutMs = 60_000,
        stepMs = 250,
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
            `waitFor("${label}") timed out after ${timeoutMs}ms${lastErr ? `: ${String(lastErr)}` : ''}`,
        );
    }

    before(async function () {
        this.timeout(3 * 60_000);

        if (!fs.existsSync(synclineBin)) {
            throw new Error(
                `syncline binary missing at ${synclineBin} - run "cargo build --release --bin syncline" from the repo root`,
            );
        }
        if (fs.existsSync(dbPath)) fs.unlinkSync(dbPath);
        if (fs.existsSync(cliFolderPath)) fs.rmSync(cliFolderPath, { recursive: true, force: true });
        fs.mkdirSync(cliFolderPath, { recursive: true });

        // -------- 1. server ----------------------------------------------
        let serverOut = '';
        serverProc = spawn(
            synclineBin,
            ['server', '--port', String(port), '--db-path', dbPath, '--log-level', 'info'],
            { stdio: 'pipe' },
        );
        serverProc.stdout?.on('data', (d) => {
            serverOut += d;
        });
        serverProc.stderr?.on('data', (d) => {
            serverOut += d;
        });
        await waitFor('server listening', async () => /listening/i.test(serverOut), 30_000, 100);

        // -------- 2. CLI peer with an empty folder -----------------------
        let cliOut = '';
        cliProc = spawn(
            synclineBin,
            ['sync', '-f', cliFolderPath, '-u', serverUrl, '--name', 'cli-peer', '--log-level', 'info'],
            { stdio: 'pipe' },
        );
        cliProc.stdout?.on('data', (d) => {
            cliOut += d;
        });
        cliProc.stderr?.on('data', (d) => {
            cliOut += d;
        });
        await waitFor('CLI handshake', async () => /v1 handshake OK/.test(cliOut), 30_000, 100);

        // -------- 3. seed Obsidian's vault BEFORE the plugin is enabled --
        const vaultPath: string = await browser.executeObsidian(
            async ({ app }) => (app as any).vault.adapter.basePath as string,
        );
        for (const item of userContent) {
            const dst = join(vaultPath, item.rel);
            fs.mkdirSync(dirname(dst), { recursive: true });
            fs.writeFileSync(dst, item.bytes);
        }
        // Also seed the device-local file. Obsidian may overwrite it
        // when it shuts down; doesn't matter, the assertion is just
        // "this file never reached the CLI peer".
        const dlDst = join(vaultPath, deviceLocal.rel);
        fs.mkdirSync(dirname(dlDst), { recursive: true });
        fs.writeFileSync(dlDst, deviceLocal.bytes);

        // Give Obsidian's vault watcher a moment to register the new
        // files. The plugin's scan also runs on connect, so even if
        // the watcher missed an event, scan_once will pick everything
        // up.
        await browser.pause(1500);

        // -------- 4. enable + connect plugin ----------------------------
        const obsidianPage = browser.getObsidianPage();
        try {
            await obsidianPage.enablePlugin('syncline');
        } catch (e: any) {
            console.error('Could not enable plugin or already enabled:', e?.message);
        }
        await browser.executeObsidian(async ({ app }, url) => {
            const plugin: any = (app as any).plugins.plugins['syncline'];
            if (!plugin) throw new Error('Syncline plugin not found in app.plugins');
            plugin.settings.serverUrl = url;
            await plugin.saveSettings();
            plugin.disconnect();
            await plugin.connect();
        }, serverUrl);
        await waitFor(
            'plugin connected',
            async () =>
                browser.executeObsidian(async ({ app }) => {
                    const plugin: any = (app as any).plugins.plugins['syncline'];
                    return !!(plugin && plugin.client && plugin.client.isConnected());
                }),
            30_000,
            200,
        );
    });

    after(() => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        if (serverProc && !serverProc.killed) serverProc.kill();
    });

    it('plugin pushes pre-existing notes + attachments to the CLI peer', async function () {
        // Plugin's first scan + WS push of three small files should
        // settle in well under a minute, but Obsidian's launch is
        // slow on cold runs - give it a roomy budget.
        this.timeout(3 * 60_000);

        await waitFor(
            'all user-content items materialised on CLI peer',
            async () => {
                for (const item of userContent) {
                    const dst = join(cliFolderPath, item.rel);
                    if (!fs.existsSync(dst)) return false;
                    if (fileSha(dst) !== expectedSha(item)) return false;
                }
                return true;
            },
            150_000,
            500,
        );

        // Per-file byte assertions for clear failure messages.
        for (const item of userContent) {
            const dst = join(cliFolderPath, item.rel);
            expect(fs.existsSync(dst)).toBe(true);
            expect(fileSha(dst)).toBe(expectedSha(item));
        }
    });

    it('device-local workspace.json never propagates', async () => {
        // The previous test waited for full convergence on the user
        // content, so by now any racy ".obsidian/workspace.json"
        // sync would already have happened. A short extra pause
        // guards against the unlikely case the plugin does a delayed
        // configSync pass.
        await browser.pause(2000);
        const dst = join(cliFolderPath, deviceLocal.rel);
        expect(fs.existsSync(dst)).toBe(false);
    });

    it('bidirectional sync still works after onboarding', async function () {
        this.timeout(60_000);

        // CLI side adds a fresh note. Plugin should pick it up via
        // the manifest broadcast and reify it in Obsidian's vault.
        const cliRel = 'notes/daily/from-cli.md';
        const cliBytes = Buffer.from('# from CLI\nhi obsidian\n');
        fs.mkdirSync(join(cliFolderPath, dirname(cliRel)), { recursive: true });
        fs.writeFileSync(join(cliFolderPath, cliRel), cliBytes);

        const vaultPath: string = await browser.executeObsidian(
            async ({ app }) => (app as any).vault.adapter.basePath as string,
        );

        await waitFor(
            'CLI add propagates into Obsidian vault',
            async () => {
                const dst = join(vaultPath, cliRel);
                if (!fs.existsSync(dst)) return false;
                return fileSha(dst) === crypto.createHash('sha256').update(cliBytes).digest('hex');
            },
            45_000,
            500,
        );

        // Obsidian side modifies an existing pre-seeded note via the
        // vault API (so it goes through Obsidian's own write path,
        // matching what real users do via the editor).
        const newBody = 'hello from pre-existing vault\nappended via Obsidian editor\n';
        await browser.executeObsidian(async ({ app }, body) => {
            const f = (app as any).vault.getAbstractFileByPath('welcome.md');
            if (!f) throw new Error('welcome.md not found in vault');
            await (app as any).vault.modify(f, body);
        }, newBody);

        const expectedHash = crypto.createHash('sha256').update(newBody).digest('hex');
        await waitFor(
            'Obsidian edit propagates back to CLI peer',
            async () => {
                const dst = join(cliFolderPath, 'welcome.md');
                if (!fs.existsSync(dst)) return false;
                return fileSha(dst) === expectedHash;
            },
            45_000,
            500,
        );
    });

    it('`syncline verify` reports convergence on the CLI peer', async function () {
        this.timeout(60_000);

        // Stop the live sync so verify can read the local state without
        // contention. Reuse the syncline binary in `verify` mode.
        if (cliProc && !cliProc.killed) cliProc.kill();
        await browser.pause(1500);

        await new Promise<void>((resolve, reject) => {
            const verify = spawn(
                synclineBin,
                ['verify', '-f', cliFolderPath, '-u', serverUrl, '--timeout-secs', '10', '--log-level', 'info'],
                { stdio: 'pipe' },
            );
            let buf = '';
            verify.stdout?.on('data', (d) => {
                buf += d;
            });
            verify.stderr?.on('data', (d) => {
                buf += d;
            });
            verify.on('exit', (code) => {
                if (code === 0 && /converged/i.test(buf)) {
                    resolve();
                } else {
                    reject(
                        new Error(
                            `verify exited with code=${code}\nstdout/stderr:\n${buf}`,
                        ),
                    );
                }
            });
        });
    });
});
