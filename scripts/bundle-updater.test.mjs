import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { EventEmitter } from 'node:events';
import { bundleUpdater } from './bundle-updater.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const require = createRequire(import.meta.url);

test('distributed MacUpdater uses the first-party adapter, complete notices and actual config/staging reads', async t => {
  const outputDirectory = await mkdtemp(join(tmpdir(), 'cublet-updater-'));
  t.after(() => rm(outputDirectory, { recursive: true, force: true }));
  const result = await bundleUpdater({ root, outputDirectory });
  assert.ok(Object.keys(result.metafile.inputs).includes('electron/updater-lazy.cjs'));
  assert.ok(!Object.keys(result.metafile.inputs).some(input => input.includes('node_modules/lazy-val/')));
  assert.ok(!Object.values(result.metafile.outputs).some(output => output.imports.some(input => input.external && /^lazy-val(?:\/|$)/.test(input.path))));
  assert.deepEqual(result.notices.warnings, []);
  assert.equal(result.notices.packages.length, 15);
  assert.ok(result.notices.packages.every(item => !item.missingLicenseNotice));
  const notices = await readFile(join(outputDirectory, 'THIRD_PARTY_NOTICES.txt'), 'utf8');
  assert.ok(!notices.includes('lazy-val@'));
  assert.ok(!notices.includes(root));
  const source = await readFile(join(outputDirectory, 'updater.cjs'), 'utf8');
  assert.ok(!source.includes('node_modules/lazy-val/'));
  assert.doesNotMatch(source, /require\(["']lazy-val(?:["']|\/)/);

  // Only Electron's native event surface is synthetic. MacUpdater, config YAML
  // parsing, lazy promises, staging-ID filesystem IO and provider logic are real.
  const nativeUpdater = new EventEmitter();
  const runtime = { exports: {} };
  new Function('require', 'module', 'exports', '__filename', '__dirname', source)(
    name => name === 'electron' ? { autoUpdater: nativeUpdater } : require(name),
    runtime, runtime.exports, join(outputDirectory, 'updater.cjs'), outputDirectory,
  );
  const updater = new runtime.exports.MacUpdater(null, {
    version: '0.6.1', isPackaged: true, userDataPath: outputDirectory,
    baseCachePath: outputDirectory, name: 'Cublet', whenReady: async () => {},
  });
  updater.logger = null;
  updater.autoDownload = false;
  updater.autoInstallOnAppQuit = false;
  const first = join(outputDirectory, 'first.yml');
  const second = join(outputDirectory, 'second.yml');
  await writeFile(first, 'provider: generic\nurl: https://example.invalid/first/\nupdaterCacheDirName: cublet-first\n');
  await writeFile(second, 'provider: generic\nurl: https://example.invalid/second/\nupdaterCacheDirName: cublet-second\n');
  updater.updateConfigPath = first;
  const configPromise = updater.configOnDisk.value;
  assert.equal(updater.configOnDisk.value, configPromise);
  assert.equal((await configPromise).updaterCacheDirName, 'cublet-first');
  updater.updateConfigPath = join(outputDirectory, 'missing.yml');
  const failed = updater.configOnDisk.value;
  await assert.rejects(failed, /ENOENT/);
  assert.equal(updater.configOnDisk.value, failed);
  updater.updateConfigPath = second;
  assert.notEqual(updater.configOnDisk.value, failed);
  assert.equal((await updater.configOnDisk.value).updaterCacheDirName, 'cublet-second');
  const stagingPromise = updater.stagingUserIdPromise.value;
  assert.equal(updater.stagingUserIdPromise.value, stagingPromise);
  const stagingId = await stagingPromise;
  assert.match(stagingId, /^[\da-f-]{36}$/i);
  assert.equal(await readFile(join(outputDirectory, '.updaterId'), 'utf8'), stagingId);

  let checks = 0;
  updater.httpExecutor = { request: async () => { checks++; return 'version: 0.6.1\nfiles:\n  - url: Cublet-0.6.1-arm64.zip\n    sha512: fixture\n'; } };
  const checked = await updater.checkForUpdates();
  assert.equal(checked.updateInfo.version, '0.6.1');
  assert.equal(checked.downloadPromise, undefined);
  assert.equal(checks, 1);
  assert.equal(updater.autoInstallOnAppQuit, false);
  nativeUpdater.removeAllListeners();
});

test('updater upgrades require an explicit adapter contract review', async t => {
  const fixtureRoot = await mkdtemp(join(tmpdir(), 'cublet-updater-version-'));
  t.after(() => rm(fixtureRoot, { recursive: true, force: true }));
  const packageDirectory = join(fixtureRoot, 'node_modules/electron-updater');
  await mkdir(packageDirectory, { recursive: true });
  await writeFile(join(packageDirectory, 'package.json'), JSON.stringify({ name: 'electron-updater', version: '99.0.0' }));
  await assert.rejects(bundleUpdater({ root: fixtureRoot }), /Audit the first-party lazy adapter contract/);
});
