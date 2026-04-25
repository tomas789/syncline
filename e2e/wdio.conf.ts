export const config = {
    runner: 'local',
    autoCompileOpts: {
        autoCompile: true,
        tsNodeOpts: {
            transpileOnly: true,
            project: 'tsconfig.json'
        }
    },
    specs: ['./test/specs/**/*.ts'],
    exclude: [],
    maxInstances: 1,
    capabilities: [{
        browserName: 'obsidian',
        'wdio:obsidianOptions': {
            plugins: [`${__dirname}/../obsidian-plugin`],
            vault: `${__dirname}/test-vault`
        }
    }],
    logLevel: 'info',
    bail: 0,
    baseUrl: 'http://localhost',
    waitforTimeout: 10000,
    connectionRetryTimeout: 120000,
    connectionRetryCount: 3,
    services: ['obsidian'],
    framework: 'mocha',
    reporters: ['spec'],
    mochaOpts: {
        ui: 'bdd',
        // Per-test budget. The cold-build setup runs in `before()` (which
        // has its own 180s timeout); each test only waits for one
        // cross-process sync round-trip and finishes in seconds.
        // Linux GHA runners occasionally drift past the previous 120s
        // budget on the "propagates deletes CLI → Obsidian" test (the
        // unlink → 5s scanner poll → manifest broadcast → plugin
        // reconcile → vault trash chain stacks several debounces).
        // 240s gives generous headroom without changing semantics.
        timeout: 240000
    },
};
