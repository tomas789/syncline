const { _electron: electron } = require('playwright');
const path = require('path');
const fs = require('fs');

(async () => {
    const vaultPath = path.join(__dirname, 'test-vault-pw');
    if (!fs.existsSync(vaultPath)) fs.mkdirSync(vaultPath);
    if (!fs.existsSync(path.join(vaultPath, '.obsidian'))) fs.mkdirSync(path.join(vaultPath, '.obsidian'));
    fs.writeFileSync(path.join(vaultPath, 'test.md'), '# Hello World');

    const appPaths = [
        '/Applications/Obsidian.app/Contents/MacOS/Obsidian',
    ];
    let executablePath = appPaths.find(p => fs.existsSync(p));

    console.log('Launching obsidian at', executablePath, 'with vault', vaultPath);

    try {
        const electronApp = await electron.launch({
            executablePath,
            args: [vaultPath]
        });

        const window = await electronApp.firstWindow();
        console.log('Window opened');
        
        // Let it run a bit
        await new Promise(r => setTimeout(r, 3000));
        
        const title = await window.title();
        console.log('Window title:', title);

        const text = await window.innerText('body');
        console.log('Body text first 200 chars:', text.substring(0, 200).replace(/\n/g, ' '));
        
        await electronApp.close();
        console.log('Success');
    } catch (e) {
        console.error('Failed to launch or interact:', e);
    }
})();
