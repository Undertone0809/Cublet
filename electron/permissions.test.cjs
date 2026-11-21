const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { createDiskPermissions, FULL_DISK_ACCESS_SETTINGS } = require('./permissions.cjs');

function fixture(overrides = {}) {
  const calls = [];
  const fileSystem = {
    openSync: (file, flags) => { calls.push(['open', file, flags]); return 42; },
    fstatSync: (fd) => { calls.push(['stat', fd]); return { isFile: () => true }; },
    closeSync: (fd) => calls.push(['close', fd]),
  };
  const permissions = createDiskPermissions({
    app: { isPackaged: true, getName: () => 'Mac 磁盘管家', getPath: (name) => {
      assert.equal(name, 'exe');
      return '/Applications/Mac 磁盘管家.app/Contents/MacOS/Mac 磁盘管家';
    } },
    shell: { openExternal: async () => {} }, platform: 'darwin',
    fileSystem, userHome: () => '/Users/test', now: () => new Date('2026-11-10T01:00:00.000Z'),
    ...overrides,
  });
  return { permissions, calls, fileSystem };
}
function failure(code) { return Object.assign(new Error('sensitive path or contents must not escape'), { code }); }

test('readable FDA sentinel grants effective access using only read-only open/stat/close in this process', () => {
  const { permissions, calls } = fixture();
  const status = permissions.getDiskPermissions();
  assert.equal(status.status, 'granted');
  assert.equal(status.canVerify, true);
  assert.equal(status.checkedAt, '2026-11-10T01:00:00.000Z');
  assert.deepEqual(status.evidence, { kind: 'full-disk-access-sentinel', result: 'readable' });
  assert.equal(status.appPath, '/Applications/Mac 磁盘管家.app');
  assert.equal(status.relaunchRecommended, false);
  assert.deepEqual(calls, [
    ['open', '/Users/test/Library/Application Support/com.apple.TCC/TCC.db', fs.constants.O_RDONLY | fs.constants.O_NONBLOCK | 0x20000000],
    ['stat', 42], ['close', 42],
  ]);
});

test('privacy rejection is denied; missing files, symlinks, POSIX denial and internal failures remain unknown', () => {
  for (const [code, status, result] of [
    ['EPERM', 'denied', 'blocked'], ['EACCES', 'unknown', 'filesystem-denied'],
    ['ENOENT', 'unknown', 'unavailable'], ['ENOTDIR', 'unknown', 'unavailable'],
    ['ELOOP', 'unknown', 'unavailable'], ['EIO', 'unknown', 'error'],
  ]) {
    const { permissions } = fixture({ fileSystem: { openSync: () => { throw failure(code); } } });
    const info = permissions.getDiskPermissions();
    assert.equal(info.status, status, code);
    assert.equal(info.evidence.result, result, code);
    assert.equal(info.canVerify, status !== 'unknown');
    assert.equal(JSON.stringify(info).includes('sensitive path or contents'), false);
  }
});

test('refresh sees real permission changes and does not retain a previous grant', () => {
  const { permissions, fileSystem, calls } = fixture();
  assert.equal(permissions.getDiskPermissions().status, 'granted');
  fileSystem.openSync = () => { throw failure('EPERM'); };
  assert.equal(permissions.getDiskPermissions({ refresh: true }).status, 'denied');
  fileSystem.openSync = () => { calls.push(['open-again']); return 43; };
  assert.equal(permissions.getDiskPermissions().status, 'granted');
  assert.deepEqual(calls.at(-1), ['close', 43]);
});

test('unexpected descriptor metadata never grants and still closes the handle', () => {
  for (const fstatSync of [() => ({ isFile: () => false }), () => { throw failure('EIO'); }]) {
    const { permissions, fileSystem, calls } = fixture();
    fileSystem.fstatSync = fstatSync;
    assert.equal(permissions.getDiskPermissions().status, 'unknown');
    assert.deepEqual(calls.at(-1), ['close', 42]);
  }
});

test('opening settings cannot grant permission; successful recheck clears relaunch recommendation', async () => {
  const opened = [];
  const { permissions, fileSystem } = fixture({ shell: { openExternal: async (url) => opened.push(url) } });
  fileSystem.openSync = () => { throw failure('EPERM'); };
  assert.deepEqual(await permissions.openDiskPrivacySettings(), { opened: true, relaunchRecommended: true });
  assert.deepEqual(opened, [FULL_DISK_ACCESS_SETTINGS]);
  assert.equal(permissions.getDiskPermissions().status, 'denied');
  assert.equal(permissions.getDiskPermissions().relaunchRecommended, true);
  fileSystem.openSync = () => 42;
  assert.equal(permissions.getDiskPermissions().status, 'granted');
  assert.equal(permissions.getDiskPermissions().relaunchRecommended, false);
});

test('unsupported platforms never probe or launch settings; failed launch does not alter state', async () => {
  const { permissions: unsupported, calls } = fixture({ platform: 'linux', shell: { openExternal: () => { throw Error('must not launch'); } } });
  assert.equal(unsupported.getDiskPermissions().status, 'unknown');
  assert.equal(unsupported.getDiskPermissions().evidence.result, 'unsupported');
  assert.deepEqual(calls, []);
  assert.deepEqual(await unsupported.openDiskPrivacySettings(), { opened: false, relaunchRecommended: false });
  const { permissions: failed } = fixture({ shell: { openExternal: async () => { throw Error('unavailable'); } } });
  assert.deepEqual(await failed.openDiskPrivacySettings(), { opened: false, relaunchRecommended: false });
  assert.equal(failed.getDiskPermissions().relaunchRecommended, false);
});
