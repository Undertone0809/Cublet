import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, cp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { createRequire } from 'node:module';
import { stageDistributionNotices, verifyDistributionNotices } from './distribution-notices.mjs';
const require = createRequire(import.meta.url);
const preserveElectronNotices = require('./preserve-electron-notices.cjs');
const { Arch } = require('builder-util');

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'cublet-distribution-notices-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const staged = join(root, '.build/arm64/licenses');
  const extracted = join(root, 'extracted');
  await mkdir(staged, { recursive: true });
  await mkdir(extracted);
  await writeFile(join(staged, 'README.txt'), 'Generated dependency license gate');
  const context = { electronPlatformName: 'darwin', arch: Arch.arm64, appOutDir: extracted, packager: { projectDir: root, info: { framework: { version: '99.1.2' } } } };
  return { root, staged, extracted, context };
}

test('afterExtract preserves the exact target SDK license bytes and version, without host fallback', async t => {
  const { staged, extracted, context } = await fixture(t);
  const chromium = '<html>Exact target Chromium notices\n</html>\n';
  await writeFile(join(extracted, 'LICENSE'), 'Exact target Electron license\n');
  await writeFile(join(extracted, 'LICENSES.chromium.html'), chromium);
  await preserveElectronNotices(context);
  assert.equal(await readFile(join(staged, 'Electron-LICENSE.txt'), 'utf8'), 'Exact target Electron license\n');
  assert.equal(await readFile(join(staged, 'LICENSES.chromium.html'), 'utf8'), chromium);
  assert.deepEqual(JSON.parse(await readFile(join(staged, 'Electron-SDK.json'), 'utf8')), { version: '99.1.2', platform: 'darwin', arch: 'arm64' });
  await rm(join(extracted, 'LICENSES.chromium.html'));
  await assert.rejects(preserveElectronNotices(context), /ENOENT/);
  await writeFile(join(extracted, 'LICENSES.chromium.html'), ' \n');
  await assert.rejects(preserveElectronNotices(context), /Empty extracted Electron notice/);
});

test('final distribution gate rejects missing or changed delivered bytes', async t => {
  const { root, staged } = await fixture(t);
  const resources = join(root, 'Resources');
  await mkdir(resources);
  await writeFile(join(staged, 'Electron-SDK.json'), JSON.stringify({ arch: 'arm64', platform: 'darwin', version: '99' }));
  await cp(staged, join(resources, 'licenses'), { recursive: true });
  await verifyDistributionNotices({ root, arch: 'arm64', resources });
  await writeFile(join(resources, 'licenses/README.txt'), 'Changed');
  await assert.rejects(verifyDistributionNotices({ root, arch: 'arm64', resources }), /Packaged notice mismatch/);
  await rm(join(resources, 'licenses/README.txt'));
  await assert.rejects(verifyDistributionNotices({ root, arch: 'arm64', resources }), /ENOENT/);
});

test('unsupported target fails before deleting or writing staged licenses', async t => {
  const { root, staged } = await fixture(t);
  await assert.rejects(stageDistributionNotices({ root, arch: 'arm64', target: 'x86_64-apple-darwin' }), /Unsupported notice target/);
  assert.equal(await readFile(join(staged, 'README.txt'), 'utf8'), 'Generated dependency license gate');
});
