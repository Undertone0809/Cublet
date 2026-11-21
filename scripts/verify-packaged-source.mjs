import { readdir, readFile } from 'node:fs/promises';
import { join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const asar = require('@electron/asar');
const root = fileURLToPath(new URL('..', import.meta.url));
const archive = process.argv[2];
if (!archive) throw new Error('usage: verify-packaged-source.mjs APP_ASAR');
let count = 0;
async function walk(dir) {
  for (const item of await readdir(join(root,dir), {withFileTypes:true})) {
    const file = join(dir,item.name);
    if (item.isDirectory()) await walk(file);
    else {
      if (file.startsWith('electron/') && (file.endsWith('.test.cjs') || file === 'electron/dev.cjs')) continue;
      const actual = asar.extractFile(archive, file);
      if (!actual.equals(await readFile(join(root,file)))) throw new Error(`Packaged bytes differ from prepared source/build: ${file}`);
      count++;
    }
  }
}
for (const dir of ['dist','electron','electron-runtime']) await walk(dir);
const packaged = JSON.parse(asar.extractFile(archive,'package.json').toString());
const source = JSON.parse(await readFile(join(root,'package.json'),'utf8'));
if (packaged.version !== source.version || packaged.name !== source.name) throw new Error('Packaged application identity differs from source');
if (process.argv.includes('--official') && packaged.cubletOfficialRelease !== true) throw new Error('Missing official update channel in release candidate');
console.log(`Verified ${count} renderer/native runtime files against the prepared source/build.`);
