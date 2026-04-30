// Cross-vault community-plugin installation via Syncline.
//
// Real user story:
//   "I installed the Tasks plugin in my desktop vault. I want to open
//    Obsidian on my laptop and have Tasks already there."
//
// What we drive end-to-end:
//   1. Vault A (desktop) has the plugin's files on disk under
//      `.obsidian/plugins/<id>/` and ticks the per-plugin
//      `configSync.communityPluginData[<id>]` checkbox in the Syncline
//      settings.
//   2. Syncline pushes those files to the server.
//   3. A separate CLI peer attached to the same server witnesses the
//      upload byte-identical (proxy for "another always-on device").
//   4. We reboot Obsidian into a brand-new empty vault B with Syncline
//      pre-installed but no plugins or config carried over.
//   5. Vault B's Syncline points at the same server with the same
//      per-plugin opt-in. The plugin's files materialise on vault B's
//      disk.
//   6. Triggering `app.plugins.loadManifests()` on vault B surfaces
//      the new plugin in `app.plugins.manifests` - the same registry
//      Obsidian's "Installed plugins" list reads from. By Obsidian's
//      own definition the plugin is now installed on vault B.
//
// Caveat: we use a synthesised "Tasks-like" plugin (id `e2e-fake-tasks`)
// with a minimal but valid manifest.json + main.js + data.json. Using
// the real Tasks bundle would tie the test to a network download and
// to a specific upstream version - what we want to exercise is the
// SYNC, not the plugin runtime. The shape of the files is exactly
// what Obsidian's plugin loader expects.

import { spawn, ChildProcess } from 'child_process';
import { join, dirname } from 'path';
import * as fs from 'fs';
import * as os from 'os';
import * as crypto from 'crypto';
import { expect, browser } from '@wdio/globals';

describe('Syncline - community plugin installs cross-vault via the network', () => {
    const port = 3072;
    const serverUrl = `ws://localhost:${port}/sync`;
    const repoRoot = join(__dirname, '../../../');
    const e2eDir = join(__dirname, '../../');
    const dbPath = join(e2eDir, 'plugin-cross-vault.db');
    const cliFolderPath = join(e2eDir, 'plugin-cross-vault-cli');
    const synclineBin = join(repoRoot, 'target/release/syncline');

    // The plugin we are "installing" in vault A and expecting to see in vault B.
    const PLUGIN_ID = 'e2e-fake-tasks';
    type Item = { rel: string; bytes: Buffer };
    const pluginFiles: Item[] = [
        {
            rel: `.obsidian/plugins/${PLUGIN_ID}/manifest.json`,
            bytes: Buffer.from(
                JSON.stringify({
                    id: PLUGIN_ID,
                    name: 'E2E Fake Tasks',
                    version: '0.0.1',
                    minAppVersion: '1.0.0',
                    description: 'Synthesised stand-in for the Tasks plugin used by Syncline e2e.',
                    author: 'syncline-e2e',
                    isDesktopOnly: false,
                }),
            ),
        },
        {
            rel: `.obsidian/plugins/${PLUGIN_ID}/main.js`,
            // Minimal-but-valid Obsidian plugin body. We don't load it
            // (loadManifests() only reads manifest.json), but ship it
            // to mirror a real plugin's on-disk shape.
            bytes: Buffer.from(
                "'use strict';\nObject.defineProperty(exports, '__esModule', { value: true });\n" +
                    "const obsidian = require('obsidian');\n" +
                    'class FakeTasksPlugin extends obsidian.Plugin {\n' +
                    '  async onload() {}\n' +
                    '  async onunload() {}\n' +
                    '}\n' +
                    'module.exports = FakeTasksPlugin;\n',
            ),
        },
        {
            rel: `.obsidian/plugins/${PLUGIN_ID}/data.json`,
            bytes: Buffer.from(JSON.stringify({ tasks: [], counter: 0 })),
        },
    ];

    // A blank vault layout we'll feed to `reloadObsidian({ vault: ... })`
    // for phase B. Has to be a real path on disk; wdio-obsidian-service
    // copies it before opening, so anything we leave here stays
    // unchanged across runs.
    const vaultBSrc = fs.mkdtempSync(join(os.tmpdir(), 'syncline-vaultB-src-'));
    fs.mkdirSync(join(vaultBSrc, '.obsidian'), { recursive: true });
    // A throwaway note so Obsidian considers the vault non-empty.
    fs.writeFileSync(
        join(vaultBSrc, 'README.md'),
        '# vault B\nthis is the laptop side\n',
    );

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

    /** Set the Syncline plugin's serverUrl + per-plugin opt-in,
     *  reconnect, and wait for `isConnected()`. Idempotent - safe to
     *  call from both vault A and vault B's Obsidian. */
    async function configureAndConnectSyncline(): Promise<void> {
        const obsidianPage = browser.getObsidianPage();
        try {
            await obsidianPage.enablePlugin('syncline');
        } catch (e: any) {
            console.error('enablePlugin (syncline) said:', e?.message);
        }
        await browser.executeObsidian(
            async ({ app }, [url, optedId]) => {
                const plugin: any = (app as any).plugins.plugins['syncline'];
                if (!plugin) throw new Error('Syncline plugin not loaded');
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
            [serverUrl, PLUGIN_ID] as [string, string],
        );
        await waitFor(
            'Syncline plugin connected',
            async () =>
                browser.executeObsidian(async ({ app }) => {
                    const plugin: any = (app as any).plugins.plugins['syncline'];
                    return !!(plugin && plugin.client && plugin.client.isConnected());
                }),
            30_000,
            200,
        );
    }

    before(async function () {
        this.timeout(3 * 60_000);

        if (!fs.existsSync(synclineBin)) {
            throw new Error(
                `syncline binary missing at ${synclineBin} - run "cargo build --release --bin syncline"`,
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

        // Persistent CLI peer.
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
    });

    after(() => {
        if (cliProc && !cliProc.killed) cliProc.kill();
        if (serverProc && !serverProc.killed) serverProc.kill();
        try {
            fs.rmSync(vaultBSrc, { recursive: true, force: true });
        } catch {}
    });

    it('vault A pushes the plugin files to the server (CLI peer witnesses)', async function () {
        this.timeout(3 * 60_000);

        // Drop the plugin into vault A (the default vault Obsidian was
        // launched with by the wdio config).
        const vaultAPath: string = await browser.executeObsidian(
            async ({ app }) => (app as any).vault.adapter.basePath as string,
        );
        for (const item of pluginFiles) {
            const dst = join(vaultAPath, item.rel);
            fs.mkdirSync(dirname(dst), { recursive: true });
            fs.writeFileSync(dst, item.bytes);
        }
        await browser.pause(1500);

        await configureAndConnectSyncline();

        // CLI peer should now have the plugin files byte-identical.
        await waitFor(
            'plugin files visible on CLI peer',
            async () => {
                for (const item of pluginFiles) {
                    const dst = join(cliFolderPath, item.rel);
                    if (!fs.existsSync(dst)) return false;
                    if (fileSha(dst) !== expectedSha(item)) return false;
                }
                return true;
            },
            150_000,
            500,
        );
        for (const item of pluginFiles) {
            expect(fileSha(join(cliFolderPath, item.rel))).toBe(expectedSha(item));
        }
    });

    it('a fresh Obsidian vault sees the plugin appear via Syncline alone', async function () {
        this.timeout(5 * 60_000);

        // Reboot Obsidian into a fresh empty vault B with Syncline
        // re-enabled. wdio-obsidian-service copies vaultBSrc, so vault
        // A's files are out of the picture - the only path the plugin
        // can reach vault B is through the server.
        await browser.reloadObsidian({
            vault: vaultBSrc,
            plugins: ['syncline'],
        });

        const vaultBPath: string = await browser.executeObsidian(
            async ({ app }) => (app as any).vault.adapter.basePath as string,
        );

        // Sanity: vault B starts without the plugin's directory.
        expect(fs.existsSync(join(vaultBPath, '.obsidian/plugins', PLUGIN_ID))).toBe(false);

        // Configure Syncline on vault B (same server, same opt-in).
        await configureAndConnectSyncline();

        // Plugin files materialise on vault B's disk via Syncline alone.
        await waitFor(
            'plugin files materialise on vault B',
            async () => {
                for (const item of pluginFiles) {
                    const dst = join(vaultBPath, item.rel);
                    if (!fs.existsSync(dst)) return false;
                    if (fileSha(dst) !== expectedSha(item)) return false;
                }
                return true;
            },
            150_000,
            500,
        );

        // Now ask Obsidian to rescan its plugins folder. The newly
        // arrived plugin must show up in the same registry the
        // "Installed plugins" UI reads from.
        const installed: any = await browser.executeObsidian(
            async ({ app }, id) => {
                const p: any = (app as any).plugins;
                if (typeof p.loadManifests === 'function') {
                    await p.loadManifests();
                } else if (typeof p.loadManifest === 'function') {
                    // Older Obsidians used a singular form. Either is
                    // fine for the test - we just need the new
                    // manifest registered.
                    await p.loadManifest();
                }
                const m = p.manifests?.[id];
                return m
                    ? { id: m.id, name: m.name, version: m.version }
                    : null;
            },
            PLUGIN_ID,
        );
        expect(installed).not.toBeNull();
        expect(installed.id).toBe(PLUGIN_ID);
        expect(installed.version).toBe('0.0.1');
        expect(installed.name).toBe('E2E Fake Tasks');
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
                            `verify exited code=${code}\nstdout/stderr:\n${buf}`,
                        ),
                    );
                }
            });
        });
    });
});
