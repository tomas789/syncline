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
        // Tests that wait for cross-process sync (server ↔ CLI ↔ plugin)
        // need headroom on cold CI runners where the first text-file
        // round-trip through the v1 content-subdoc path is slow.
        timeout: 180000
    },
};
