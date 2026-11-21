'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { test } = require('node:test');
const { createAppUninstaller, readOpenPaths } = require('./app-uninstall.cjs');

const BUNDLE_ID = 'com.example.fixture';

async function fixture(t) {
  const root = await fs.mkdtemp(path.join(await fs.realpath(os.tmpdir()), 'app-uninstall-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  const applicationsRoot = path.join(root, 'Applications');
  const trash = path.join(root, 'Trash');
  const ownExecutable = path.join(applicationsRoot, 'Storage Manager.app', 'Contents', 'MacOS', 'Storage Manager');
  await Promise.all([home, applicationsRoot, trash].map(directory => fs.mkdir(directory)));
  const app = { getPath: key => ({ home, exe: ownExecutable })[key] };
  const moved = [];
  const shell = {
    async trashItem(target) {
      await fs.rename(target, path.join(trash, String(moved.length)));
      moved.push(target);
    },
  };
  const options = {
    app, shell, home, applicationsRoot, platform: 'darwin',
    plistString: async (plist, key) => JSON.parse(await fs.readFile(plist, 'utf8'))[key],
    sizeOf: async () => 4096, openPaths: async () => [], indexedApplications: async () => [], darwinDirectories: async () => [],
  };
  return { home, root, applicationsRoot, ownExecutable, options, moved };
}

async function makeApp(target, id = BUNDLE_ID) {
  const macOS = path.join(target, 'Contents', 'MacOS');
  await fs.mkdir(macOS, { recursive: true });
  await fs.writeFile(path.join(macOS, 'Fixture'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
  await fs.writeFile(path.join(target, 'Contents', 'Info.plist'), JSON.stringify({
    CFBundleIdentifier: id, CFBundlePackageType: 'APPL', CFBundleExecutable: 'Fixture',
  }));
  return target;
}

async function makeResidue(home, directory, leaf = BUNDLE_ID, type = 'directory') {
  const parent = path.join(home, 'Library', directory);
  await fs.mkdir(parent, { recursive: true });
  const target = path.join(parent, leaf);
  if (type === 'file') await fs.writeFile(target, 'fixture');
  else await fs.mkdir(target);
  return target;
}

test('installed app rows use actionable paths and keep shared data visible', async t => {
  const { applicationsRoot, home, ownExecutable, options } = await fixture(t);
  const service = createAppUninstaller(options);
  const app = path.join(applicationsRoot, 'Utilities', 'Example.app');
  const userApp = path.join(home, 'Applications', 'Example.app');
  const build = path.join(home, 'build', 'Example.app');
  const own = path.resolve(ownExecutable, '../../..');
  const shared = path.join(home, 'Library', 'Application Support', BUNDLE_ID);
  const alias = '/System/Volumes/Data' + app;
  const row = (path, associatedPaths = [], bundleId) => ({ path, associatedPaths, bundleId });
  const rows = [
    row(app, [shared]), row(alias, [shared]), row(userApp), row(build, [shared]),
    row(path.join(app, 'Contents', 'Helper.app')),
    row(path.join(applicationsRoot, 'Old.app.backup', 'Helper.app')),
    row(path.join(applicationsRoot, 'PlugIn.bundle', 'Helper.app')),
    row(own), row(path.join(applicationsRoot, 'Renamed.app'), [], 'com.zeeland.codex-storage-manager.electron'),
    row('/System/Applications/Calendar.app'),
  ];
  assert.deepEqual(service.listApplications(rows).map(({ path, sharedData }) => [path, sharedData]), [
    [app, true], [userApp, false],
  ]);
  assert.deepEqual(service.listApplications(rows, '/System/Volumes/Data' + applicationsRoot)
    .map(({ path, sharedData }) => [path, sharedData]), [[app, true]]);
  assert.equal(service.listApplications([rows[0], rows[1]])[0].sharedData, false);
});

test('plans allowlisted app data only and consume an authorized plan exactly once', async t => {
  const { home, applicationsRoot, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Utilities', 'Test App.app'));
  const cache = await makeResidue(home, 'Caches');
  const preference = await makeResidue(home, 'Preferences', BUNDLE_ID + '.plist', 'file');
  const fuzzy = await makeResidue(home, 'Caches', 'Test App');
  const container = await makeResidue(home, 'Group Containers');
  const service = createAppUninstaller(options);
  const plan = await service.preview([app, app]);
  assert.deepEqual(plan.items.map(({ path, kind, bytes, blockedReason }) => ({ path, kind, bytes, blockedReason })), [
    { path: app, kind: 'app', bytes: 4096, blockedReason: undefined },
    { path: cache, kind: 'cache', bytes: 4096, blockedReason: undefined },
    { path: preference, kind: 'preferences', bytes: 4096, blockedReason: undefined },
  ]);
  await assert.rejects(service.execute(plan.planId, [plan.items[0].id, 'forged']), /请选择预览/);
  const result = await service.execute(plan.planId, [plan.items[1].id, plan.items[0].id]);
  assert.deepEqual(result, { mode: 'trash', movedPaths: [app, cache], movedAppPaths: [app], failures: [] });
  assert.deepEqual(moved, [app, cache]);
  await Promise.all([preference, fuzzy, container].map(target => fs.access(target)));
  await assert.rejects(service.execute(plan.planId, [plan.items[0].id]), /预览已失效/);
});

test('protects nested bundles, symlink ancestors, system paths, and the app itself', async t => {
  const { applicationsRoot, home, ownExecutable, root, options, moved } = await fixture(t);
  const primary = await makeApp(path.join(applicationsRoot, 'Parent.app'));
  const nested = await makeApp(path.join(primary, 'Contents', 'Helpers', 'Helper.app'));
  const backup = await makeApp(path.join(applicationsRoot, 'Parent.app.backup'));
  const backupHelper = await makeApp(path.join(backup, 'Contents', 'Helpers', 'Helper.app'));
  const outside = await makeApp(path.join(root, 'External', 'External.app'));
  const own = await makeApp(path.dirname(path.dirname(path.dirname(ownExecutable))));
  const ownCopy = await makeApp(path.join(home, 'Applications', 'Copy.app'), 'com.zeeland.codex-storage-manager.electron');
  await fs.symlink(path.dirname(outside), path.join(applicationsRoot, 'Linked Folder'));
  const symlinkAncestor = path.join(applicationsRoot, 'Linked Folder', 'External.app');
  const alias = path.join(applicationsRoot, 'Alias.app');
  await fs.symlink(outside, alias);
  const plain = path.join(applicationsRoot, 'Fake.app');
  await fs.mkdir(plain);
  const service = createAppUninstaller(options);
  const plan = await service.preview([nested, backupHelper, outside, own, ownCopy, symlinkAncestor, alias, plain,
    '/System/Applications/Calendar.app', '/System/Volumes/Data/Applications/Other.app']);
  assert.equal(plan.items.length, 10);
  assert.ok(plan.items.every(item => item.kind === 'app' && item.blockedReason));
  assert.equal((await service.execute(plan.planId, plan.items.map(item => item.id))).failures.length, 10);
  assert.deepEqual(moved, []);
});

test('a user-installed Apple bundle never offers Apple Library paths', async t => {
  const { home, applicationsRoot, options } = await fixture(t);
  const id = 'COM.Apple.Example';
  const app = await makeApp(path.join(applicationsRoot, 'Apple Fixture.app'), id);
  const cache = await makeResidue(home, 'Caches', id);
  const service = createAppUninstaller(options);
  const plan = await service.preview([app]);
  assert.deepEqual(plan.items.map(item => item.path), [app]);
  await service.execute(plan.planId, [plan.items[0].id]);
  await fs.access(cache);
});

test('protects symlinked or hardlinked Library entries and a file opened by another process', async t => {
  const { home, applicationsRoot, root, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  const linkParent = path.join(home, 'Library', 'Caches');
  await fs.mkdir(linkParent, { recursive: true });
  await fs.symlink(root, path.join(linkParent, BUNDLE_ID));
  const preference = await makeResidue(home, 'Preferences', BUNDLE_ID + '.plist', 'file');
  await fs.link(preference, path.join(root, 'Hardlink.plist'));
  const log = await makeResidue(home, 'Logs', BUNDLE_ID + '.log', 'file');
  const service = createAppUninstaller({ ...options, openPaths: async () => [log] });
  const plan = await service.preview([app]);
  assert.deepEqual(plan.items.map(item => item.kind), ['app', 'cache', 'logs', 'preferences']);
  assert.ok(plan.items.slice(1).every(item => item.blockedReason));
  const result = await service.execute(plan.planId, plan.items.map(item => item.id));
  assert.equal(result.failures.length, 3);
  assert.deepEqual(moved, [app]);
  await Promise.all([log, preference].map(target => fs.access(target)));
});

test('a same-ID installed, renamed, inventory, or indexed copy protects related data', async t => {
  const { home, applicationsRoot, root, options } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  await makeResidue(home, 'Application Support');
  const standard = await makeApp(path.join(home, 'Applications', 'Subdirectory', 'Another.app'), BUNDLE_ID.toUpperCase());
  const backup = await makeApp(path.join(applicationsRoot, 'Renamed.app.backup'));
  const inventory = await makeApp(path.join(root, 'Other', 'Inventory.app'));
  const indexed = await makeApp(path.join(root, 'Other', 'Indexed.app'));
  const service = createAppUninstaller({
    ...options,
    indexedApplications: async () => [indexed],
  });
  const available = async () => {
    const plan = await service.preview([app]);
    assert.equal(plan.items[0].blockedReason, undefined);
    return plan.items[1].blockedReason;
  };
  for (const copy of [standard, backup, indexed]) {
    assert.match(await available(), /应用副本/);
    await fs.rm(copy, { recursive: true });
  }
  assert.equal(await available(), undefined);
  service.rememberApplications([{ path: inventory }]);
  assert.match(await available(), /应用副本/);
});

test('a truncated scan inventory stays diagnostic after a successful target lookup', async t => {
  const { home, applicationsRoot, options } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  await makeResidue(home, 'Application Support');
  const service = createAppUninstaller(options);
  service.rememberApplications([], true);
  const plan = await service.preview([app]);
  assert.equal(plan.items[0].blockedReason, undefined);
  assert.equal(plan.items[1].blockedReason, undefined);
  assert.equal(plan.items[1].copyProtection, undefined);
  assert.deepEqual(plan.sharedCopies, undefined);
  assert(plan.coverage.some(row => row.source === 'past-inventory' && row.partial));
  const result = await service.execute(plan.planId, plan.items.map(item => item.id));
  assert.deepEqual(result.failures, []);
});

test('rechecks process evidence, stale paths, expiry, and identity before mutation', async t => {
  const { applicationsRoot, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  let paths = [];
  let currentTime = 0;
  let processChecks = 0;
  const service = createAppUninstaller({
    ...options,
    now: () => currentTime,
    openPaths: async () => {
      processChecks++;
      if (!paths) throw new Error('native check unavailable');
      return paths;
    },
  });
  let plan = await service.preview([app]);
  paths = [path.join(app, 'Contents', 'Frameworks', 'Helper.app', 'Contents', 'MacOS', 'Helper')];
  let result = await service.execute(plan.planId, [plan.items[0].id]);
  assert.match(result.failures[0].message, /辅助进程/);
  paths = ['/private/var/folders/qa/AppTranslocation/darwin/d/Fixture.app/Contents/MacOS/Fixture'];
  assert.match((await service.preview([app])).items[0].blockedReason, /辅助进程/);
  paths = null;
  assert.match((await service.preview([app])).items[0].blockedReason, /无法确认应用运行状态/);
  paths = [];
  plan = await service.preview([app]);
  currentTime += 6 * 60_000;
  await assert.rejects(service.execute(plan.planId, [plan.items[0].id]), /预览已失效/);
  plan = await service.preview([app]);
  await fs.rename(app, app + '.old');
  await makeApp(app);
  paths = [path.join(app, 'Contents', 'MacOS', 'Fixture')];
  const checksBeforeStalePlan = processChecks;
  result = await service.execute(plan.planId, [plan.items[0].id]);
  assert.match(result.failures[0].message, /已发生变化/);
  assert.equal(processChecks, checksBeforeStalePlan, 'A stale plan must not inspect an unapproved replacement');
  assert.deepEqual(moved, []);
});

test('replacement during native usage inspection is rejected before Trash', async t => {
  const { applicationsRoot, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Inspection Race.app'));
  let replaceDuringCheck = false;
  const service = createAppUninstaller({ ...options, openPaths: async () => {
    if (replaceDuringCheck) {
      replaceDuringCheck = false;
      await fs.rename(app, app + '.old');
      await makeApp(app);
    }
    return [];
  } });
  const plan = await service.preview([app]);
  replaceDuringCheck = true;
  const result = await service.execute(plan.planId, [plan.items[0].id]);
  assert.match(result.failures[0].message, /已发生变化/);
  assert.deepEqual(moved, []);
  await fs.access(app);
});

test('prevents optional data moves when the app was skipped or failed, and reports partial success', async t => {
  const { home, applicationsRoot, options, moved } = await fixture(t);
  const brokenApp = await makeApp(path.join(applicationsRoot, 'Permission Denied.app'));
  const normalApp = await makeApp(path.join(applicationsRoot, 'Normal.app'), 'com.example.normal');
  const cache = await makeResidue(home, 'Caches');
  const service = createAppUninstaller({
    ...options,
    shell: {
      async trashItem(target) {
        if (target === brokenApp) throw new Error('permission denied');
        await options.shell.trashItem(target);
      },
    },
  });
  let plan = await service.preview([brokenApp]);
  let result = await service.execute(plan.planId, [plan.items[1].id]);
  assert.equal(result.failures.length, 1);
  assert.match(result.failures[0].message, /请先将对应应用/);
  plan = await service.preview([brokenApp, normalApp]);
  result = await service.execute(plan.planId, plan.items.map(item => item.id));
  assert.deepEqual(moved, [normalApp]);
  assert.deepEqual(result.movedAppPaths, [normalApp]);
  assert.equal(result.failures.length, 2);
  assert.equal(result.failures[0].message, 'permission denied');
  await fs.access(cache);
});

test('rechecks optional data identity and live copies after moving the bundle', async t => {
  const { home, applicationsRoot, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  const other = await makeApp(path.join(applicationsRoot, 'Other.app'), 'com.example.other');
  const cache = await makeResidue(home, 'Caches');
  const otherCache = await makeResidue(home, 'Caches', 'com.example.other');
  const service = createAppUninstaller({
    ...options,
    shell: {
      async trashItem(target) {
        await options.shell.trashItem(target);
        if (target === app) await makeApp(path.join(home, 'Applications', 'New Copy.app'));
        if (target === other) {
          await fs.rename(otherCache, otherCache + '.old');
          await fs.mkdir(otherCache);
        }
      },
    },
  });
  const plan = await service.preview([app, other]);
  assert.equal(plan.items.filter(item => item.blockedReason).length, 0);
  const result = await service.execute(plan.planId, plan.items.map(item => item.id));
  assert.deepEqual(moved, [app, other]);
  assert.equal(result.failures.length, 2);
  assert.match(result.failures[0].message, /应用副本/);
  assert.match(result.failures[1].message, /已发生变化/);
  await Promise.all([cache, otherCache].map(target => fs.access(target)));
});

test('checks for a new copy after checking process evidence for each data item', async t => {
  const { home, applicationsRoot, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  const cache = await makeResidue(home, 'Caches');
  const logs = await makeResidue(home, 'Logs');
  const newCopy = path.join(home, 'Applications', 'New Copy.app');
  const service = createAppUninstaller({
    ...options,
    openPaths: async () => {
      if (moved.includes(cache)) await makeApp(newCopy);
      return [];
    },
  });
  const plan = await service.preview([app]);
  const result = await service.execute(plan.planId, plan.items.map(item => item.id));
  assert.deepEqual(result.movedPaths, [app, cache]);
  assert.equal(result.failures[0].path, logs);
  assert.match(result.failures[0].message, /应用副本/);
  await fs.access(logs);
});

test('scopes lsof to validated directories and files while retaining executable evidence', async t => {
  const { applicationsRoot, home } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  const preference = await makeResidue(home, 'Preferences', BUNDLE_ID + '.plist', 'file');
  const helper = path.join(app, 'Contents', 'Helpers', 'Helper');
  const calls = [];
  const paths = await readOpenPaths([app, preference, app], async (command, args, options) => {
    calls.push({ command, args, options });
    if (command === '/bin/ps') return { stdout: `  ${helper}\n/usr/bin/sleep\n`, stderr: '' };
    return { stdout: `p123\0\nf4\0n${preference}\0\n`, stderr: '' };
  });
  assert.deepEqual(paths, [helper, '/usr/bin/sleep', preference].map(value => value.normalize('NFD').toLowerCase()));
  assert.deepEqual(calls[1].args, ['-nP', '-F0fn', '+D', app, '--', preference]);
  assert.equal(calls[1].options.timeout, 6000);
});

test('accepts only completed scoped no-match results and fails closed on diagnostics or failed evidence', async t => {
  const { applicationsRoot } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  const ps = { stdout: '/usr/bin/sleep\n', stderr: '' };
  const noMatch = () => Object.assign(new Error('no matching file'), { code: 1, killed: false, signal: null, stdout: '', stderr: '' });
  const runWith = result => async command => {
    if (command === '/bin/ps') return ps;
    if (result instanceof Error) throw result;
    return result;
  };
  assert.deepEqual(await readOpenPaths([app], runWith(noMatch())), ['/usr/bin/sleep']);
  for (const result of [
    Object.assign(noMatch(), { stderr: 'lsof: WARNING: Output information may be incomplete.' }),
    Object.assign(noMatch(), { killed: true, signal: 'SIGTERM' }),
    Object.assign(noMatch(), { code: 'ENOBUFS' }),
    Object.assign(noMatch(), { code: 'ENOENT' }),
    { stdout: '', stderr: 'lsof: WARNING: unable to stat selected directory' },
    { stdout: 'truncated process evidence', stderr: '' },
  ]) await assert.rejects(readOpenPaths([app], runWith(result)));
  await assert.rejects(readOpenPaths([app], async () => ({ stdout: '', stderr: '' })), /无法确认应用运行状态/);
  const alias = path.join(applicationsRoot, 'Alias.app');
  await fs.symlink(app, alias);
  await assert.rejects(readOpenPaths([alias], runWith(noMatch())), /无法确认应用运行状态/);
});

test('native scoped evidence detects an open descendant without a running app executable', { skip: process.platform !== 'darwin' }, async t => {
  const { applicationsRoot } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  const target = path.join(app, 'Contents', 'held-resource');
  await fs.writeFile(target, 'held open');
  const handle = await fs.open(target, 'r');
  try {
    assert((await readOpenPaths([app])).includes(target.normalize('NFD').toLowerCase()));
  } finally {
    await handle.close();
  }
});

test('a fresh preview recovers after unavailable evidence without authorizing its older blocked plan', async t => {
  const { applicationsRoot, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Fixture.app'));
  let available = false;
  const targets = [];
  const service = createAppUninstaller({ ...options, openPaths: async paths => {
    targets.push(paths);
    if (!available) throw new Error('native check timed out');
    return [];
  } });
  const blocked = await service.preview([app]);
  assert.match(blocked.items[0].blockedReason, /无法确认应用运行状态/);
  available = true;
  const refreshed = await service.preview([app]);
  assert.notEqual(refreshed.planId, blocked.planId);
  assert.notEqual(refreshed.items[0].id, blocked.items[0].id);
  assert.equal(refreshed.items[0].blockedReason, undefined);
  const staleResult = await service.execute(blocked.planId, [blocked.items[0].id]);
  assert.match(staleResult.failures[0].message, /无法确认应用运行状态/);
  assert.deepEqual(moved, []);
  assert.deepEqual((await service.execute(refreshed.planId, [refreshed.items[0].id])).movedPaths, [app]);
  assert(targets.every(paths => paths.length === 1 && paths[0] === app));
});

test('binds explicit product confirmation to disposition and revision, freezes the started mode', async t => {
  const { applicationsRoot, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Disposition.app'));
  let policy = { mode: 'permanent', revision: 'one' };
  const calls = [];
  const service = createAppUninstaller({ ...options, getDisposition: async () => policy,
    dispose: async (item, mode, verification) => {
      assert.equal(typeof verification.verify, 'function');
      assert(verification.stamps.every(stamp => typeof stamp.path === 'string' && typeof stamp.fingerprint === 'string'));
      await verification.verify();
      calls.push(mode);
      policy = { mode: 'trash', revision: 'three' };
      await options.shell.trashItem(item.path);
    },
  });
  let preview = await service.preview([target]);
  assert.equal(preview.mode, 'permanent');
  policy = { mode: 'permanent', revision: 'two' };
  await assert.rejects(service.execute(preview.planId, [preview.items[0].id], 'permanent'), /处理方式/);
  assert.deepEqual(moved, []);
  preview = await service.preview([target]);
  await assert.rejects(service.execute(preview.planId, [preview.items[0].id]), /确认方式/);
  preview = await service.preview([target]);
  const result = await service.execute(preview.planId, [preview.items[0].id], 'permanent');
  assert.equal(result.mode, 'permanent');
  assert.deepEqual(calls, ['permanent']);
  assert.deepEqual(result.movedPaths, [target]);
});

test('fails closed on unreadable settings and refuses permanent disposal without its native backend', async t => {
  const { applicationsRoot, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Native Missing.app'));
  const unreadable = createAppUninstaller({ ...options, getDisposition: async () => ({ mode: 'invalid', revision: 'one' }) });
  await assert.rejects(unreadable.preview([target]), /无法读取处理方式/);
  const service = createAppUninstaller({ ...options, getDisposition: async () => ({ mode: 'permanent', revision: 'one' }) });
  const preview = await service.preview([target]);
  const result = await service.execute(preview.planId, [preview.items[0].id], 'permanent');
  assert.match(result.failures[0].message, /永久删除服务不可用/);
  assert.deepEqual(moved, []);
  await fs.access(target);
});

test('discovers exact HTTP, ByHost, embedded helpers, Darwin and verified Containers while protecting weak names', async t => {
  const { applicationsRoot, home, root, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Example.app'));
  const http = await makeResidue(home, 'HTTPStorages');
  const state = await makeResidue(home, 'Saved Application State', BUNDLE_ID + '.savedState');
  const byHost = await makeResidue(home, 'Preferences/ByHost', BUNDLE_ID + '.12345678-1234-1234-1234-1234567890AB.plist', 'file');
  const wrongHost = await makeResidue(home, 'Preferences/ByHost', BUNDLE_ID + '.other.12345678-1234-1234-1234-1234567890AB.plist', 'file');
  const helperId = BUNDLE_ID + '.helper';
  await makeApp(path.join(target, 'Contents', 'Frameworks', 'Example Helper.app'), helperId);
  const helperCache = await makeResidue(home, 'Caches', helperId);
  const framework = path.join(target, 'Contents', 'Frameworks', 'Squirrel.framework');
  const updater = path.join(framework, 'Versions', 'A', 'Resources', 'ShipIt');
  await fs.mkdir(path.dirname(updater), { recursive: true });
  await fs.writeFile(updater, 'fixture', { mode: 0o755 });
  await fs.symlink('A', path.join(framework, 'Versions', 'Current'));
  await fs.symlink('Versions/Current/Resources', path.join(framework, 'Resources'));
  const shipit = await makeResidue(home, 'Caches', BUNDLE_ID + '.ShipIt');
  const darwin = path.join(root, 'Darwin');
  await fs.mkdir(darwin);
  const darwinCache = path.join(darwin, helperId);
  await fs.mkdir(darwinCache);
  const container = await makeResidue(home, 'Containers');
  await fs.writeFile(path.join(container, '.com.apple.containermanagerd.metadata.plist'), JSON.stringify({ MCMMetadataIdentifier: BUNDLE_ID }));
  const nameOnly = await makeResidue(home, 'Application Support', 'Example');
  const originalName = await makeResidue(home, 'Application Support', 'Original Product');
  const plist = path.join(target, 'Contents', 'Info.plist');
  const bundle = JSON.parse(await fs.readFile(plist, 'utf8'));
  await fs.writeFile(plist, JSON.stringify({ ...bundle, CFBundleName: 'Original Product' }));
  const groupContainer = await makeResidue(home, 'Group Containers');
  const service = createAppUninstaller({ ...options, darwinDirectories: async () => [darwin] });
  const preview = await service.preview([target]);
  const rows = new Map(preview.items.map(item => [item.path, item]));
  for (const value of [http, state, byHost, helperCache, shipit, darwinCache, container]) {
    assert(rows.has(value), value);
    assert.equal(rows.get(value).blockedReason, undefined, value);
    assert.equal(rows.get(value).evidenceStrength, 'strong');
  }
  assert.match(rows.get(nameOnly).blockedReason, /归属线索/);
  assert.match(rows.get(originalName).blockedReason, /归属线索/);
  assert.equal(rows.get(nameOnly).protected, true);
  assert(!rows.has(wrongHost));
  assert(!rows.has(groupContainer));
});

test('only previously published local external apps and verified backups become uninstall targets', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const outside = await makeApp(path.join(home, 'Downloads', 'Old.app.backup'));
  const cloud = await makeApp(path.join(home, 'Library', 'CloudStorage', 'Provider', 'Cloud.app'));
  const trash = await makeApp(path.join(home, '.Trash', 'Discarded.app'));
  const service = createAppUninstaller(options);
  assert((await service.preview([outside])).items[0].blockedReason);
  service.rememberApplications([{ path: outside }, { path: cloud }, { path: trash }, { path: '/System/Applications/Calendar.app' }]);
  const rows = service.listApplications([{ path: outside, associatedPaths: [] }, { path: cloud, associatedPaths: [] }, { path: trash, associatedPaths: [] }]);
  assert.deepEqual(rows.map(row => row.path), [outside]);
  const preview = await service.preview([outside, cloud, trash, '/System/Applications/Calendar.app']);
  assert.equal(preview.items[0].blockedReason, undefined);
  assert(preview.items.slice(1).every(row => row.blockedReason));
  const result = await service.execute(preview.planId, preview.items.map(row => row.id));
  assert.deepEqual(moved, [outside]);
  assert.equal(result.failures.length, 3);
  assert(!moved.includes(applicationsRoot));
});

test('ordinary files do not consume the application-directory sharing budget', async t => {
  const { applicationsRoot, home, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Budget.app'));
  const cache = await makeResidue(home, 'Caches');
  // Reproduce the old global 6,000-entry failure with inert files, not bundles.
  const files = Array.from({ length: 6050 }, (_, index) => path.join(applicationsRoot, `ordinary-${index}`));
  for (let index = 0; index < files.length; index += 100) await Promise.all(files.slice(index, index + 100).map(file => fs.writeFile(file, '')));
  const preview = await createAppUninstaller(options).preview([target]);
  assert.equal(preview.items.find(item => item.path === cache).blockedReason, undefined);
  assert(preview.coverage.filter(row => row.source === 'application-root').every(row => row.partial === false));
});

test('keeps discovered copies and per-root coverage when another root is partial', async t => {
  const { applicationsRoot, home, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Coverage.app'));
  const cache = await makeResidue(home, 'Caches');
  const other = await makeApp(path.join(home, 'Applications', 'Copy.app'));
  await fs.mkdir(path.join(applicationsRoot, ...Array(18).fill('Deep')), { recursive: true });
  const service = createAppUninstaller(options);
  let preview = await service.preview([target]);
  assert.match(preview.items.find(item => item.path === cache).blockedReason, /应用副本/);
  assert(preview.coverage.some(row => row.root === applicationsRoot && row.partial));
  assert(preview.coverage.some(row => row.root === path.join(home, 'Applications') && !row.partial));
  await fs.rm(other, { recursive: true });
  preview = await service.preview([target]);
  assert.equal(preview.items.find(item => item.path === cache).blockedReason, undefined);
  assert(preview.coverage.some(row => row.root === applicationsRoot && row.partial));
});

test('seals permanent preview entries privately and blocks unavailable tree evidence', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Sealed.app'));
  const cache = await makeResidue(home, 'Caches');
  const sealed = [];
  const service = createAppUninstaller({ ...options,
    getDisposition: async () => ({ mode: 'permanent', revision: 'one' }),
    sealApplicationItem: async stamps => {
      sealed.push(stamps[0].path);
      if (stamps[0].path === cache) throw new Error('unreadable tree');
      return 'owned-fixture-tree-seal';
    },
    dispose: async (item, mode, verification) => {
      assert.equal(mode, 'permanent');
      assert.equal(verification.seal, 'owned-fixture-tree-seal');
      await options.shell.trashItem(item.path);
    },
  });
  const preview = await service.preview([target]);
  assert.deepEqual(new Set(sealed), new Set([target, cache]));
  assert(!JSON.stringify(preview).includes('owned-fixture-tree-seal'));
  assert.match(preview.items.find(item => item.path === cache).blockedReason, /目录边界/);
  const result = await service.execute(preview.planId, preview.items.map(item => item.id), 'permanent');
  assert.deepEqual(moved, [target]);
  assert.equal(result.failures.length, 1);
  await fs.access(cache);
});

test('serializes execute while asynchronous policy read is pending', async t => {
  const { applicationsRoot, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Serialized.app'));
  let waitForPolicy;
  const service = createAppUninstaller({ ...options, getDisposition: async () => {
    if (waitForPolicy) await new Promise(resolve => { waitForPolicy = resolve; });
    return { mode: 'trash', revision: 'one' };
  } });
  const preview = await service.preview([target]);
  waitForPolicy = true;
  const first = service.execute(preview.planId, [preview.items[0].id], 'trash');
  await assert.rejects(service.execute(preview.planId, [preview.items[0].id], 'trash'), /另一项/);
  waitForPolicy();
  await first;
  assert.deepEqual(moved, [target]);
});

test('counts a shared related path once when previewing two application copies', async t => {
  const { applicationsRoot, home, options } = await fixture(t);
  const first = await makeApp(path.join(applicationsRoot, 'First.app'));
  const second = await makeApp(path.join(applicationsRoot, 'Second.app'));
  const cache = await makeResidue(home, 'Caches');
  const preview = await createAppUninstaller(options).preview([first, second]);
  const rows = preview.items.filter(item => item.path === cache);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].bytes, 4096);
  assert.equal(rows[0].shared, true);
  assert.match(rows[0].blockedReason, /应用副本/);
  assert.deepEqual(rows[0].sharedWithAppPaths, [second]);
});

test('published apps outside the current home or inside Library remain protected', async t => {
  const { root, home, options, moved } = await fixture(t);
  const outside = await makeApp(path.join(root, 'Other Account', 'Other.app'));
  const library = await makeApp(path.join(home, 'Library', 'Application Support', 'Embedded.app'));
  const service = createAppUninstaller(options);
  service.rememberApplications([{ path: outside }, { path: library }]);
  const preview = await service.preview([outside, library]);
  assert(preview.items.every(item => item.blockedReason));
  const result = await service.execute(preview.planId, preview.items.map(item => item.id));
  assert.equal(result.failures.length, 2);
  assert.deepEqual(moved, []);
  await Promise.all([outside, library].map(target => fs.access(target)));
});

test('does not report success when a disposer leaves the original item in place', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Noop.app'));
  const cache = await makeResidue(home, 'Caches');
  const service = createAppUninstaller({ ...options, dispose: async () => {} });
  const preview = await service.preview([target]);
  const result = await service.execute(preview.planId, preview.items.map(item => item.id));
  assert.deepEqual(result.movedPaths, []);
  assert.equal(result.failures.length, 2);
  assert.match(result.failures[0].message, /仍然存在/);
  assert.match(result.failures[1].message, /请先将对应应用/);
  assert.deepEqual(moved, []);
  await Promise.all([target, cache].map(value => fs.access(value)));
});

test('rechecks only selected failed related data after verified application removal, with repeat recovery', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Recovery.app'));
  const cache = await makeResidue(home, 'Caches');
  const logs = await makeResidue(home, 'Logs');
  let allowData = false;
  let policy = { mode: 'trash', revision: 'first' };
  const seals = [];
  const service = createAppUninstaller({ ...options,
    getDisposition: async () => policy,
    sealApplicationItem: async stamps => { seals.push(stamps[0].path); return 'fresh-owned-seal'; },
    dispose: async (item, mode, verification) => {
      if (item.kind !== 'app' && !allowData) throw new Error('fixture permission denied');
      if (mode === 'permanent') assert.equal(verification.seal, 'fresh-owned-seal');
      await options.shell.trashItem(item.path);
    },
  });
  const first = await service.preview([target]);
  const selected = first.items.filter(item => item.kind === 'app' || item.path === cache).map(item => item.id);
  const result = await service.execute(first.planId, selected, 'trash');
  assert.equal(result.recoverableRelated, true);
  assert.deepEqual(result.movedAppPaths, [target]);
  assert.deepEqual(result.failures.map(item => item.path), [cache]);
  await makeResidue(home, 'HTTPStorages');
  policy = { mode: 'permanent', revision: 'second' };
  const retry = await service.preview([target], first.planId);
  assert.equal(retry.mode, 'permanent');
  assert.equal(retry.items[0].alreadyRemoved, true);
  assert.equal(retry.items[0].bytes, 0);
  assert.deepEqual(retry.items.slice(1).map(item => item.path), [cache]);
  assert.deepEqual(seals, [cache]);
  const retryResult = await service.execute(retry.planId, [retry.items[1].id], 'permanent');
  assert.equal(retryResult.recoverableRelated, true);
  assert.deepEqual(retryResult.movedAppPaths, []);
  assert.deepEqual(moved, [target]);
  allowData = true;
  const final = await service.preview([target], retry.planId);
  const finalResult = await service.execute(final.planId, [final.items[1].id], 'permanent');
  assert.deepEqual(finalResult.movedPaths, [cache]);
  assert.equal(finalResult.recoverableRelated, undefined);
  assert.deepEqual(moved, [target, cache]);
  await fs.access(logs);
});

test('recovery refuses stale receipts, app restoration, selection expansion, and replacement data', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Recovery Guard.app'));
  const cache = await makeResidue(home, 'Caches');
  let time = 0;
  const service = createAppUninstaller({ ...options, now: () => time,
    getDisposition: async () => ({ mode: 'trash', revision: 'one' }),
    dispose: async item => { if (item.kind === 'app') await options.shell.trashItem(item.path); else throw new Error('fixture failure'); },
  });
  const first = await service.preview([target]);
  await service.execute(first.planId, first.items.map(item => item.id), 'trash');
  await assert.rejects(service.preview([target], 'forged'), /恢复记录/);
  await assert.rejects(service.preview([path.join(applicationsRoot, 'Other.app')], first.planId), /恢复范围/);
  await makeApp(target);
  await assert.rejects(service.preview([target], first.planId), /仍然存在/);
  await fs.rm(target, { recursive: true });
  await fs.rename(cache, cache + '.original');
  await fs.mkdir(cache);
  const blocked = await service.preview([target], first.planId);
  assert.match(blocked.items[1].blockedReason, /已发生变化/);
  const result = await service.execute(blocked.planId, [blocked.items[1].id], 'trash');
  assert.deepEqual(result.movedPaths, []);
  assert.deepEqual(moved, [target]);
  time = 6 * 60_000;
  await assert.rejects(service.preview([target], first.planId), /恢复记录已失效/);
  await fs.access(cache);
});

test('mixed recovery preserves failed application selection and removed-application residuals', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const firstApp = await makeApp(path.join(applicationsRoot, 'Removed.app'));
  const secondApp = await makeApp(path.join(applicationsRoot, 'Failed.app'), 'com.example.second');
  const completedApp = await makeApp(path.join(applicationsRoot, 'Completed.app'), 'com.example.completed');
  const unselectedApp = await makeApp(path.join(applicationsRoot, 'Unselected.app'), 'com.example.unselected');
  const cache = await makeResidue(home, 'Caches');
  const otherCache = await makeResidue(home, 'Caches', 'com.example.second');
  let fail = true;
  const service = createAppUninstaller({ ...options,
    getDisposition: async () => ({ mode: 'trash', revision: 'one' }),
    dispose: async item => { if (fail && (item.kind !== 'app' || item.path === secondApp)) throw new Error('fixture failure'); await options.shell.trashItem(item.path); },
  });
  const selected = [completedApp, unselectedApp, firstApp, secondApp];
  const first = await service.preview(selected);
  await service.execute(first.planId, first.items.filter(item => item.appPath !== unselectedApp).map(item => item.id), 'trash');
  const retry = await service.preview(selected, first.planId);
  assert.deepEqual(retry.items.map(item => item.path), [firstApp, cache, secondApp, otherCache]);
  assert.equal(retry.items[0].alreadyRemoved, true);
  assert.equal(retry.items[2].alreadyRemoved, undefined);
  fail = false;
  const result = await service.execute(retry.planId, retry.items.filter(item => !item.alreadyRemoved).map(item => item.id), 'trash');
  assert.deepEqual(result.movedAppPaths, [secondApp]);
  assert.deepEqual(result.failures, []);
  assert.deepEqual(new Set(moved), new Set([completedApp, firstApp, secondApp, cache, otherCache]));
  await fs.access(unselectedApp);
});

test('product can remove a verified other Cublet bundle while own bundle and its data stay protected', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const own = await makeApp(path.join(applicationsRoot, 'Running Cublet.app'), 'com.zeeland.codex-storage-manager.electron');
  const other = await makeApp(path.join(applicationsRoot, 'Cublet QA.app'), 'com.zeeland.codex-storage-manager.electron');
  const cache = await makeResidue(home, 'Caches', 'com.zeeland.codex-storage-manager.electron');
  const service = createAppUninstaller({ ...options,
    app: { getPath: key => key === 'exe' ? path.join(own, 'Contents', 'MacOS', 'Fixture') : home },
    getDisposition: async () => ({ mode: 'trash', revision: 'one' }),
  });
  const preview = await service.preview([other, own]);
  assert.equal(preview.items.find(item => item.path === other).blockedReason, undefined);
  assert.match(preview.items.find(item => item.path === cache).blockedReason, /当前使用/);
  assert.match(preview.items.find(item => item.path === own).blockedReason, /自身/);
  const result = await service.execute(preview.planId, preview.items.filter(item => !item.blockedReason).map(item => item.id), 'trash');
  assert.deepEqual(result.movedAppPaths, [other]);
  assert.deepEqual(moved, [other]);
  await fs.access(cache);
  await fs.access(own);
});

test('caps verified embedded helper paths across directories and bounds each application plan', async t => {
  const { applicationsRoot, home, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Many Helpers.app'));
  await makeResidue(home, 'Caches');
  for (let index = 0; index < 36; index++) await makeApp(path.join(target, 'Contents', index < 18 ? 'Frameworks' : 'Helpers', `Helper ${index}.app`), `${BUNDLE_ID}.helper${index}`);
  const stamps = [];
  const service = createAppUninstaller({ ...options,
    getDisposition: async () => ({ mode: 'permanent', revision: 'one' }),
    sealApplicationItem: async value => { stamps.push(value); return 'fixture-seal'; },
  });
  const preview = await service.preview([target]);
  assert(preview.warnings.some(value => /辅助应用标识达到/.test(value)));
  assert.equal(stamps.find(value => value[0].path === target).length, 3 + 32 * 2);
});

test('a declared app group is discoverable but remains protected without verified membership', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const app = await makeApp(path.join(applicationsRoot, 'Shared Container.app'));
  const container = path.join(home, 'Library', 'Group Containers', 'group.com.example.shared');
  await fs.mkdir(container, { recursive: true });
  const service = createAppUninstaller({ ...options, declaredGroupIds: async () => ['group.com.example.shared', '../../outside'] });
  const preview = await service.preview([app]);
  const hint = preview.items.find(({ path }) => path === container);
  assert.equal(hint.evidence, 'declared-group-container');
  assert(hint.shared && hint.protected);
  const result = await service.execute(preview.planId, preview.items.map(({ id }) => id));
  assert.deepEqual(moved, [app]);
  assert(result.failures.some(({ path }) => path === container));
  await fs.access(container);
});

test('residual recovery rechecks new shared copies and running users without broadening paths', async t => {
  const { applicationsRoot, home, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Live Recovery.app'));
  const cache = await makeResidue(home, 'Caches');
  let opened = [];
  const service = createAppUninstaller({ ...options,
    openPaths: async () => opened,
    getDisposition: async () => ({ mode: 'trash', revision: 'one' }),
    dispose: async item => { if (item.kind === 'app') await options.shell.trashItem(item.path); else throw new Error('fixture failure'); },
  });
  const first = await service.preview([target]);
  await service.execute(first.planId, first.items.map(item => item.id), 'trash');
  const copy = await makeApp(path.join(applicationsRoot, 'New Copy.app'));
  let retry = await service.preview([target], first.planId);
  assert.match(retry.items[1].blockedReason, /应用副本/);
  await fs.rm(copy, { recursive: true });
  opened = [path.join(cache, 'open-file')];
  retry = await service.preview([target], first.planId);
  assert.match(retry.items[1].blockedReason, /辅助进程/);
  opened = [];
  retry = await service.preview([target], first.planId);
  assert.equal(retry.items[1].blockedReason, undefined);
  assert.deepEqual(retry.items.map(item => item.path), [target, cache]);
  assert.deepEqual(moved, [target]);
});

test('private interrupted uninstall stages never become mutation targets', async t => {
  const { applicationsRoot, options, moved } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, '.cublet-uninstall-owned-stage', 'Leftover.app'));
  const service = createAppUninstaller(options);
  service.rememberApplications([{ path: target }]);
  const preview = await service.preview([target]);
  assert(preview.items[0].blockedReason);
  await service.execute(preview.planId, [preview.items[0].id]);
  assert.deepEqual(moved, []);
  await fs.access(target);
});


test('unrelated invalid bundle metadata remains diagnostic in both disposal modes', async t => {
  for (const mode of ['trash', 'permanent']) {
    const { home, applicationsRoot, options } = await fixture(t);
    const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
    const cache = await makeResidue(home, 'Caches');
    await fs.mkdir(path.join(applicationsRoot, 'Unrelated.app', 'Contents', 'Info.plist'), { recursive: true });
    const service = createAppUninstaller({ ...options,
      getDisposition: async () => ({ mode, revision: 'one' }),
      dispose: async item => options.shell.trashItem(item.path),
    });
    const preview = await service.preview([target]);
    assert.equal(preview.items.find(item => item.path === cache).blockedReason, undefined);
    assert.equal(preview.sharedCopies, undefined);
    assert(preview.coverage.some(row => row.source === 'candidate-metadata' && row.partial));
    const result = await service.execute(preview.planId, preview.items.map(item => item.id), mode);
    assert.deepEqual(result.movedPaths, [target, cache]);
    assert.deepEqual(result.failures, []);
  }
});

test('a failed target lookup protects data, while a fresh successful preview can recover', async t => {
  const { home, applicationsRoot, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
  const cache = await makeResidue(home, 'Caches');
  let available = false;
  const service = createAppUninstaller({ ...options, indexedApplications: async () => {
    if (!available) throw new Error('query failed');
    return [];
  } });
  const blocked = await service.preview([target]);
  const item = blocked.items.find(item => item.path === cache);
  assert.match(item.blockedReason, /无法确认是否有其他应用副本/);
  assert.equal(item.copyProtection, 'unavailable');
  assert.equal(item.shared, false);
  assert.equal(blocked.sharedCopies, undefined);
  available = true;
  const preview = await service.preview([target]);
  assert.equal(preview.items.find(item => item.path === cache).copyProtection, undefined);
  assert.equal(preview.items.find(item => item.path === cache).blockedReason, undefined);
  const result = await service.execute(blocked.planId, blocked.items.map(item => item.id));
  assert.deepEqual(result.movedPaths, [target]);
  assert.equal(result.failures.length, 1);
  await fs.access(cache);
});

test('invalid indexed matching metadata fails closed, except for missing stale paths', async t => {
  const { home, root, applicationsRoot, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
  const cache = await makeResidue(home, 'Caches');
  const copy = await makeApp(path.join(root, 'Indexed.app'));
  const plist = path.join(copy, 'Contents', 'Info.plist');
  await fs.writeFile(plist, JSON.stringify({ CFBundleIdentifier: 'invalid id' }));
  const service = createAppUninstaller({ ...options, indexedApplications: async () => [copy] });
  let preview = await service.preview([target]);
  assert.equal(preview.items.find(item => item.path === cache).copyProtection, 'unavailable');
  await fs.rm(plist);
  preview = await service.preview([target]);
  assert.equal(preview.items.find(item => item.path === cache).copyProtection, 'unavailable');
  await fs.rm(copy, { recursive: true });
  preview = await service.preview([target]);
  assert.equal(preview.items.find(item => item.path === cache).blockedReason, undefined);
});

test('copy notices deduplicate physical aliases and exclude the selected bundle', async t => {
  const { home, root, applicationsRoot, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
  const copy = await makeApp(path.join(root, 'Copies', 'Actual.app'));
  const alias = path.join(root, 'Alias.app');
  const ownAlias = path.join(root, 'Own Alias.app');
  await fs.symlink(copy, alias);
  await fs.symlink(target, ownAlias);
  const cache = await makeResidue(home, 'Caches');
  const service = createAppUninstaller({ ...options, indexedApplications: async () => [alias, target, ownAlias, copy, alias] });
  const preview = await service.preview([target]);
  assert.deepEqual(preview.sharedCopies, [{ appPath: target, paths: [copy] }]);
  assert.equal(preview.items.find(item => item.path === cache).copyProtection, 'shared');
  assert.equal(preview.items.find(item => item.path === cache).shared, true);
  await fs.rm(copy, { recursive: true });
  const fresh = await service.preview([target]);
  assert.equal(fresh.sharedCopies, undefined);
  assert.equal(fresh.items.find(item => item.path === cache).blockedReason, undefined);
});

test('indexed copy matches take priority over a capped broad inventory', async t => {
  const { home, root, applicationsRoot, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
  await makeResidue(home, 'Caches');
  const indexed = await makeApp(path.join(root, 'Indexed.app'));
  const service = createAppUninstaller({ ...options, indexedApplications: async () => [indexed] });
  service.rememberApplications(Array.from({ length: 4096 }, (_, i) => ({ path: path.join(root, `Known${i}.app`) })));
  for (let i = 0; i < 1910; i += 100) {
    await Promise.all(Array.from({ length: Math.min(100, 1910 - i) }, (_, j) =>
      fs.mkdir(path.join(applicationsRoot, `Broad${i + j}.app`))));
  }
  const preview = await service.preview([target]);
  assert.deepEqual(preview.sharedCopies, [{ appPath: target, paths: [indexed] }]);
  assert.equal(preview.items[1].copyProtection, 'shared');
});

test('execute rechecks target lookup failure in both disposal modes', async t => {
  for (const mode of ['trash', 'permanent']) {
    const { home, applicationsRoot, options } = await fixture(t);
    const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
    const cache = await makeResidue(home, 'Caches');
    let available = true;
    const service = createAppUninstaller({ ...options,
      getDisposition: async () => ({ mode, revision: 'one' }),
      indexedApplications: async () => { if (!available) throw new Error('query failed'); return []; },
      dispose: async item => { await options.shell.trashItem(item.path); available = false; },
    });
    const preview = await service.preview([target]);
    const result = await service.execute(preview.planId, preview.items.map(item => item.id), mode);
    assert.deepEqual(result.movedPaths, [target]);
    assert.match(result.failures[0].message, /无法确认是否有其他应用副本/);
    await fs.access(cache);
  }
});


test('copy lookup keeps a valid different indexed ID harmless and surfaces other file blockers', async t => {
  const { home, root, applicationsRoot, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
  const unrelated = await makeApp(path.join(root, 'Indexed Other.app'), 'com.example.other');
  const cache = await makeResidue(home, 'Caches');
  let unavailable = false;
  const service = createAppUninstaller({ ...options,
    indexedApplications: async () => { if (unavailable) throw new Error('query failed'); return [unrelated]; },
    openPaths: async () => unavailable ? [cache] : [],
  });
  let preview = await service.preview([target]);
  assert.equal(preview.items.find(item => item.path === cache).blockedReason, undefined);
  unavailable = true;
  preview = await service.preview([target]);
  const item = preview.items.find(item => item.path === cache);
  assert.match(item.blockedReason, /正在使用/);
  assert.equal(item.copyProtection, undefined);
});

test('one stable notice includes verified backup and helper copies', async t => {
  const { home, root, applicationsRoot, options } = await fixture(t);
  const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
  const helperId = BUNDLE_ID + '.helper';
  await makeApp(path.join(target, 'Contents', 'Helpers', 'Embedded.app'), helperId);
  const cache = await makeResidue(home, 'Caches');
  const helperCache = await makeResidue(home, 'Caches', helperId);
  const backup = await makeApp(path.join(applicationsRoot, 'Z Backup.app.backup'));
  const helperCopy = await makeApp(path.join(root, 'A Helper.app'), helperId);
  const service = createAppUninstaller({ ...options, indexedApplications: async ids => {
    assert(ids.includes(helperId));
    return [helperCopy, backup, helperCopy];
  } });
  const preview = await service.preview([target]);
  assert.deepEqual(preview.sharedCopies, [{ appPath: target, paths: [helperCopy, backup].sort() }]);
  for (const value of [cache, helperCache]) {
    assert.equal(preview.items.find(item => item.path === value).copyProtection, 'shared');
  }
});

test('execute protects data from newly found indexed copies in both disposal modes', async t => {
  for (const mode of ['trash', 'permanent']) {
    const { home, root, applicationsRoot, options } = await fixture(t);
    const target = await makeApp(path.join(applicationsRoot, 'Selected.app'));
    const cache = await makeResidue(home, 'Caches');
    const newCopy = path.join(root, 'New Copy.app');
    let indexed = [];
    const service = createAppUninstaller({ ...options,
      getDisposition: async () => ({ mode, revision: 'one' }),
      indexedApplications: async () => indexed,
      dispose: async item => {
        await options.shell.trashItem(item.path);
        if (item.path === target) indexed = [await makeApp(newCopy)];
      },
    });
    const preview = await service.preview([target]);
    const result = await service.execute(preview.planId, preview.items.map(item => item.id), mode);
    assert.deepEqual(result.movedPaths, [target]);
    assert.match(result.failures[0].message, /应用副本/);
    await fs.access(cache);
    await fs.access(newCopy);
  }
});


test('failed or ambiguous batch lookup isolates unavailable evidence to its selected app', async t => {
  for (const cause of ['query-failure', 'invalid-metadata']) {
    const { home, root, applicationsRoot, options } = await fixture(t);
    const first = await makeApp(path.join(applicationsRoot, 'First.app'));
    const secondId = 'com.example.second';
    const second = await makeApp(path.join(applicationsRoot, 'Second.app'), secondId);
    const firstCache = await makeResidue(home, 'Caches');
    const secondCache = await makeResidue(home, 'Caches', secondId);
    const invalid = await makeApp(path.join(root, 'Unreadable Match.app'));
    await fs.writeFile(path.join(invalid, 'Contents', 'Info.plist'), JSON.stringify({ CFBundleIdentifier: 'invalid id' }));
    const queries = [];
    const service = createAppUninstaller({ ...options, indexedApplications: async ids => {
      queries.push(ids);
      if (ids.includes(BUNDLE_ID)) {
        if (cause === 'query-failure') throw new Error('first app query failed');
        return [invalid];
      }
      return [];
    } });
    const preview = await service.preview([first, second]);
    assert.equal(preview.items.find(item => item.path === firstCache).copyProtection, 'unavailable');
    assert.equal(preview.items.find(item => item.path === secondCache).blockedReason, undefined);
    assert.equal(preview.items.find(item => item.path === secondCache).copyProtection, undefined);
    assert.equal(queries.length, 3);
    assert.equal(preview.coverage.filter(row => row.source === 'application-root').length, 2);
    const result = await service.execute(preview.planId, preview.items.map(item => item.id));
    assert.deepEqual(result.movedPaths, [first, second, secondCache]);
    assert.equal(result.failures.length, 1);
    await fs.access(firstCache);
  }
});
