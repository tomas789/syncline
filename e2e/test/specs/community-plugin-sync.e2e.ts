// Community-plugin sync via configSync.communityPluginData.
//
// Realistic flow: the user has a community plugin installed in their
// Obsidian vault (say "dataview"), and wants its files - main.js,
// manifest.json, data.json - synced across devices via Syncline.
// They opt in by toggling that plugin's checkbox in the Syncline
// settings, which sets `configSync.communityPluginData[id] = true`.
//
// What this spec verifies, against real Obsidian + the WASM plugin:
//   1. An opted-in plugin's full directory under `.obsidian/plugins/<id>/`
//      lands on the CLI peer byte-identical.
//   2. A second pre-seeded plugin that the user did NOT opt in to
//      stays local - its files never reach the CLI peer.
//   3. Edits made on the CLI side to the opted-in plugin's data.json
//      propagate back into Obsidian's vault.
//   4. `syncline verify` agrees with both peers.
//
// Side notes:
//   - The plugin's hidden-file scanner runs on connect AND on a 30s
//     interval; we never have to wait that long because the initial
//     sweep happens immediately after the WS handshake.
//   - The Syncline plugin self-excludes its own directory
//     (`${configDir}/plugins/syncline/`) - this is enforced by
//     `classifyHiddenPath` returning "self" - so we don't worry
//     about its data.json racing into our test corpus.

import { spawn, ChildProcess } from 'child_process';
import { join, dirname } from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline - community plugin syncs via configSync.communityPluginData', () => {
    const port = 3071;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'community-plugin-sync.db');
    const cliFolderPath = join(e2eDir, 'community-plugin-sync-cli');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    // The user's "I want this synced" plugin.
    const optedInId = 'fake-dataview';
    // A second installed plugin the user has NOT opted in to. Its
    // files must remain local.
    const notOptedInId = 'private-plugin';

    type Item = { rel: string; bytes: Buffer };
    const optedInFiles: Item[] = [
        {
            rel: `.obsidian/plugins/${optedInId}/manifest.json`,
            bytes: Buffer.from(
                JSON.stringify({
                    id: optedInId,
                    name: 'Fake Dataview',
                    version: '0.0.1',
                    minAppVersion: '1.0.0',
                    description: 'Test fixture',
                    author: 'syncline-e2e',
                }),
            ),
        },
        {
            rel: `.obsidian/plugins/${optedInId}/main.js`,
            bytes: Buffer.from('module.exports = class {};\n// fake plugin source\n'),
        },
        {
            rel: `.obsidian/plugins/${optedInId}/data.json`,
            bytes: Buffer.from(JSON.stringify({ favoriteColor: 'syncline-cyan', counter: 0 })),
        },
    ];
    const notOptedInFiles: Item[] = [
        {
            rel: `.obsidian/plugins/${notOptedInId}/manifest.json`,
            bytes: Buffer.from(
                JSON.stringify({
                    id: notOptedInId,
                    name: 'Private Plugin',
                    version: '0.0.1',
                    minAppVersion: '1.0.0',
                    description: 'Should not sync',
                    author: 'syncline-e2e',
                }),
            ),
        },
        {
            rel: `.obsidian/plugins/${notOptedInId}/main.js`,
            bytes: Buffer.from('module.exports = class { secret(){return 42;} };\n'),
        },
    ];

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
        timeoutMs = 90_000,
        stepMs = 500,
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

        // Server.
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

        // CLI peer with empty folder.
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

        // Seed both plugins' files in Obsidian's vault before the
        // plugin connects.
        const vaultPath: string = await browser.executeObsidian(
            async ({ app }) => (app as any).vault.adapter.basePath as string,
        );
        for (const item of [...optedInFiles, ...notOptedInFiles]) {
            const dst = join(vaultPath, item.rel);
            fs.mkdirSync(dirname(dst), { recursive: true });
            fs.writeFileSync(dst, item.bytes);
        }
        await browser.pause(1500);

        // Enable + configure the Syncline plugin: opt-in only the
        // first plugin's data sync. The second one's
        // communityPluginData entry stays falsy/unset, so its files
        // must never propagate.
        const obsidianPage = browser.getObsidianPage();
        try {
            await obsidianPage.enablePlugin('syncline');
        } catch (e: any) {
            console.error('Could not enable plugin or already enabled:', e?.message);
        }
        await browser.executeObsidian(
            async ({ app }, [url, optedId]) => {
                const plugin: any = (app as any).plugins.plugins['syncline'];
                if (!plugin) throw new Error('Syncline plugin not found in app.plugins');
                plugin.settings.serverUrl = url;
                plugin.settings.configSync = {
                    ...plugin.settings.configSync,
                    communityPluginData: {
                        ...(plugin.settings.configSync?.communityPluginData ?? {}),
                        [optedId]: true,
                    },
                };
                await plugin.saveSettings();
                plugin.disconnect();
                await plugin.connect();
            },
            [serverUrl, optedInId] as [string, string],
        );
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

    it('opted-in plugin files arrive on the CLI peer byte-identical', async function () {
        this.timeout(3 * 60_000);

        await waitFor(
            'all opted-in plugin files materialised on CLI peer',
            async () => {
                for (const item of optedInFiles) {
                    const dst = join(cliFolderPath, item.rel);
                    if (!fs.existsSync(dst)) return false;
                    if (fileSha(dst) !== expectedSha(item)) return false;
                }
                return true;
            },
            150_000,
            500,
        );

        for (const item of optedInFiles) {
            const dst = join(cliFolderPath, item.rel);
            expect(fs.existsSync(dst)).toBe(true);
            expect(fileSha(dst)).toBe(expectedSha(item));
        }
    });

    it('non-opted-in plugin stays local and never propagates', async () => {
        // The previous test waited for the opted-in plugin's full
        // arrival; by now anything racing through the same scan/push
        // pipeline would already be visible. A short pause guards the
        // hidden-file-scanner's interval tick (30s) wasn't able to
        // sneak the un-opted-in plugin in afterwards.
        await browser.pause(2000);
        for (const item of notOptedInFiles) {
            const dst = join(cliFolderPath, item.rel);
            expect(fs.existsSync(dst)).toBe(
                false,
            );
        }
        // And the directory itself shouldn't have been materialised
        // either - the parent ".obsidian/plugins/" can exist (it's
        // shared with the opted-in plugin), but the un-opted-in
        // subdir must not.
        const privateDir = join(cliFolderPath, '.obsidian/plugins', notOptedInId);
        expect(fs.existsSync(privateDir)).toBe(false);
    });

    it('CLI-side edit to opted-in plugin data.json round-trips into Obsidian', async function () {
        this.timeout(60_000);

        // Modify data.json on the CLI side and watch it land in the
        // Obsidian vault.
        const target = optedInFiles.find((i) => i.rel.endsWith('data.json'))!;
        const newBody = JSON.stringify({ favoriteColor: 'syncline-cyan', counter: 7 });
        fs.writeFileSync(join(cliFolderPath, target.rel), newBody);

        const vaultPath: string = await browser.executeObsidian(
            async ({ app }) => (app as any).vault.adapter.basePath as string,
        );
        const expectedHash = crypto.createHash('sha256').update(newBody).digest('hex');
        await waitFor(
            'data.json edit reaches Obsidian vault',
            async () => {
                const dst = join(vaultPath, target.rel);
                if (!fs.existsSync(dst)) return false;
                return fileSha(dst) === expectedHash;
            },
            45_000,
            500,
        );
    });

    it('`syncline verify` reports convergence on the CLI peer', async function () {
        this.timeout(60_000);

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
