import { execFileSync } from 'node:child_process';
import { createReadStream } from 'node:fs';
import { mkdtemp, rm, stat, writeFile, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const [outputArg, version, arch] = process.argv.slice(2);
if (!outputArg || !/^\d+\.\d+\.\d+$/.test(version) || !['arm64','x64'].includes(arch)) throw new Error('usage: verify-macos-archive.mjs OUTPUT VERSION ARCH');
const output = resolve(outputArg);
const name = `Cublet-${version}-${arch}.zip`;
const zip = join(output, name);
const hash = createHash('sha512');
for await (const chunk of createReadStream(zip)) hash.update(chunk);
const sha512 = hash.digest('base64');
const size = (await stat(zip)).size;
const feed = require('js-yaml').load(await readFile(join(output, 'latest-mac.yml'), 'utf8'));
const entry = feed.files?.find(file => file.url === name || file.url?.endsWith('/'+name));
if (feed.version !== version || !entry || entry.size !== size || entry.sha512 !== sha512) throw new Error('Builder ZIP manifest differs from final ZIP bytes');
await stat(zip+'.blockmap');
const temp = await mkdtemp(join(tmpdir(), 'cublet-zip-'));
try {
  execFileSync('ditto', ['-x','-k',zip,temp]);
  execFileSync(fileURLToPath(new URL('./verify-macos-release.sh', import.meta.url)), [join(temp, 'Cublet.app'), '', arch], { stdio:'inherit', env: {...process.env, REQUIRE_SIGNED:'1', REQUIRE_NOTARIZATION:'1', EXPECTED_VERSION:version} });
  execFileSync(process.execPath, [fileURLToPath(new URL('./verify-packaged-source.mjs', import.meta.url)), join(temp, 'Cublet.app/Contents/Resources/app.asar'), '--official'], {stdio:'inherit'});
  await writeFile(join(output,'verified-archive.json'), JSON.stringify({version,arch,zip:name,size,sha512,notarizedAppVerified:true},null,2)+'\n');
} finally { await rm(temp,{recursive:true,force:true}); }
