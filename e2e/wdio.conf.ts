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
        // Per-test budget. Most tests finish in seconds; the phase1-4
        // big-vault tests use `this.timeout()` to extend per-test, but
        // some mocha+wdio versions don't honour the override on async
        // functions reliably, so we set a generous default that covers
        // the slowest legitimate path (45 min settle on a ~1300-file
        // vault). Individual fast tests still finish in milliseconds;
        // only failures pay the wait.
        timeout: 60 * 60_000
    },
};
