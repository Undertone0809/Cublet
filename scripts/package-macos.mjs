import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { createHash } from 'node:crypto';
import { sourceIdentity } from './build-source.mjs';
import { bundleUpdater } from './bundle-updater.mjs';
import { buildRendererNotices, stageDistributionNotices, verifyDistributionNotices } from './distribution-notices.mjs';
const require = createRequire(import.meta.url);

const root = fileURLToPath(new URL('..', import.meta.url));
const arch = process.env.CUBLET_ARCH || process.arch;
const target = { arm64: 'aarch64-apple-darwin', x64: 'x86_64-apple-darwin' }[arch];
if (process.platform !== 'darwin' || !target) throw new Error('Use macOS with CUBLET_ARCH=arm64 or x64');
const run = (cmd, args, env = process.env) => execFileSync(cmd, args, { cwd: root, stdio: 'inherit', env });
const source = sourceIdentity(root);
const { version } = require('../package.json');
const officialRelease = process.env.CUBLET_OFFICIAL_RELEASE === '1';
if (officialRelease && process.env.CUBLET_NOTARIZE !== '1') throw new Error('Official update-channel builds must be notarized');
run('npx', ['--no-install', 'tsc', '-b']);
const renderer = await buildRendererNotices({ root });
await bundleUpdater({ root });
run('cargo', ['build', '--locked', '--release', '--target', target, '--manifest-path', 'rust-core/Cargo.toml', '--bin', 'codex-core'], {
  ...process.env,
  CUBLET_PACKAGED_VERSION: version,
});
mkdirSync(`${root}/.build/${arch}`, { recursive: true });
copyFileSync(`${root}/rust-core/target/${target}/release/codex-core`, `${root}/.build/${arch}/codex-core`);
await stageDistributionNotices({ root, arch, target, renderer });
const builderArgs = ['--no-install', 'electron-builder', '--mac', ...(process.argv.includes('--dir') ? ['--dir'] : ['dmg', 'zip']), `--${arch}`, '--publish', 'never', `--config.directories.output=electron-dist/${arch}`];
if (officialRelease) builderArgs.push('--config.extraMetadata.cubletOfficialRelease=true');
run('npx', builderArgs);
const finishedSource = sourceIdentity(root);
if (finishedSource.inputsSha256 !== source.inputsSha256) throw new Error('Build inputs changed while packaging; discard this candidate');
const resources = `${root}/electron-dist/${arch}/${arch === 'arm64' ? 'mac-arm64' : 'mac'}/Cublet.app/Contents/Resources`;
await verifyDistributionNotices({ root, arch, resources });
const sha = file => createHash('sha256').update(readFileSync(file)).digest('hex');
writeFileSync(`${root}/electron-dist/${arch}/build-source.json`, JSON.stringify({...finishedSource, arch, asarSha256:sha(`${resources}/app.asar`), coreSha256:sha(`${resources}/bin/codex-core`), cleanerSha256:sha(`${resources}/bin/cublet-cleaner`)},null,2)+'\n');
