const { test } = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const { createUpdateController, updateFeed } = require('./updates.cjs');
const { checkpointScans, prepareUpdateShutdown, createMutationTracker } = require('./update-shutdown.cjs');
const { configureAppIdentity } = require('./app-identity.cjs');
function deferred() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
function fixture(prepareInstall = async () => {}) {
  const updater = new EventEmitter();
  const nativeUpdater = new EventEmitter();
  const calls = [];
  updater.checkForUpdates = async () => { calls.push('check'); updater.emit('update-not-available', { version: '0.5.0' }); return {}; };
  updater.downloadUpdate = async () => { calls.push('download'); updater.emit('download-progress', { percent: 34.5 }); updater.emit('update-downloaded', { version: '0.6.0' }); };
  updater.quitAndInstall = () => { calls.push('install'); nativeUpdater.on('update-downloaded', () => calls.push('native-install')); };
  const controller = createUpdateController({ updater, nativeUpdater, version: '0.5.0', enabled: true, prepareInstall });
  return { updater, nativeUpdater, calls, controller };
}
test('architecture feeds are fixed and reject nonrelease inputs', () => {
  assert.equal(updateFeed('arm64'), 'https://downloads.cublet.cc/updates/stable/arm64/');
  assert.equal(updateFeed('x64'), 'https://downloads.cublet.cc/updates/stable/x64/');
  assert.throws(() => updateFeed('../external'));
});
test('rename preserves production userData and explicit development fixture profiles', () => {
  for (const packaged of [true, false]) {
    let userData = packaged ? '/Library/AppData/cublet' : '/tmp/disposable-profile';
    let name = 'cublet';
    configureAppIdentity({ isPackaged: packaged, getPath: key => key === 'appData' ? '/Library/AppData' : userData, getName: () => name, setPath: (_key, value) => userData = value, setName: value => name = value });
    assert.equal(userData, packaged ? '/Library/AppData/mac-storage-manager' : '/tmp/disposable-profile');
    assert.equal(name, 'Cublet');
  }
});
test('explicit installed --user-data-dir remains isolated', () => {
  let profile;
  configureAppIdentity({ isPackaged: true, commandLine: { getSwitchValue: () => '/tmp/installed-acceptance' }, getPath: key => key === 'appData' ? '/Library/AppData' : '/Library/AppData/Cublet', getName: () => 'Cublet', setPath: (_key, value) => profile = value, setName() {} });
  assert.equal(profile, '/tmp/installed-acceptance');
});
test('checks never download, coalesce requests, and report latest/current/offline states', async () => {
  const { updater, controller, calls } = fixture();
  assert.equal(updater.autoDownload, false);
  assert.equal(updater.autoInstallOnAppQuit, false);
  await Promise.all([controller.check(), controller.check()]);
  assert.deepEqual(calls, ['check']);
  assert.equal(controller.snapshot().status, 'current');
  updater.checkForUpdates = async () => { throw Object.assign(new Error('net::ERR_INTERNET_DISCONNECTED'), { code: 'ERR_INTERNET_DISCONNECTED' }); };
  await controller.check();
  assert.equal(controller.snapshot().error, 'offline');
  assert.equal(controller.snapshot().retry, 'check');
});
test('download requires an available update; retries download failure and preserves progress', async () => {
  const { updater, controller, calls } = fixture();
  await controller.download();
  assert.deepEqual(calls, []);
  updater.emit('update-available', { version: '0.6.0' });
  const originalDownload = updater.downloadUpdate;
  updater.downloadUpdate = async () => { throw new Error('net::ERR_CONNECTION_RESET'); };
  assert.equal((await controller.download()).retry, 'download');
  assert.equal(controller.snapshot().error, 'offline');
  updater.downloadUpdate = originalDownload;
  const state = await controller.download();
  assert.equal(state.status, 'ready'); assert.equal(state.percent, 100); assert.equal(state.latestVersion, '0.6.0');
  await controller.check();
  assert.deepEqual(calls, ['download']);
});
test('install waits for uninstall and scan checkpoint and ignores double activation', async () => {
  const uninstall = deferred(); const checkpoint = deferred(); const order = [];
  const { controller, updater, calls } = fixture(async () => prepareUpdateShutdown({
    waitForUninstall: () => { order.push('wait-uninstall'); return uninstall.promise; },
    checkpoint: () => { order.push('checkpoint'); return checkpoint.promise; },
  }));
  await controller.install(); assert.deepEqual(calls, []);
  updater.emit('update-downloaded', { version: '0.6.0' });
  const operation = controller.install(); await controller.install();
  await Promise.resolve(); assert.deepEqual(calls, []); assert.deepEqual(order, ['wait-uninstall']);
  uninstall.resolve(); await new Promise(setImmediate); assert.deepEqual(order, ['wait-uninstall', 'checkpoint']); assert.deepEqual(calls, []);
  checkpoint.resolve(); await operation; assert.deepEqual(calls, ['install']);
});
test('install preparation failure keeps the app open and can retry', async () => {
  let fail = true;
  const { controller, updater, calls } = fixture(async () => { if (fail) throw new Error('checkpoint failed'); });
  updater.emit('update-downloaded', { version: '0.6.0' });
  assert.equal((await controller.install()).status, 'error'); assert.deepEqual(calls, []);
  fail = false; await controller.install(); assert.deepEqual(calls, ['install']);
});
test('asynchronous native signature/network failure clears only attempted install callbacks', async () => {
  const { controller, updater, nativeUpdater, calls } = fixture();
  const readiness = () => {}; nativeUpdater.on('update-downloaded', readiness);
  updater.emit('update-downloaded', { version: '0.6.0' });
  await controller.install();
  updater.emit('error', new Error('Code signature did not pass validation'));
  assert.equal(controller.snapshot().status, 'error'); assert.equal(controller.snapshot().error, 'signature');
  assert.deepEqual(nativeUpdater.listeners('update-downloaded'), [readiness]);
  await controller.install(); nativeUpdater.emit('update-downloaded');
  assert.deepEqual(calls, ['install', 'install', 'native-install']);
});
test('checkpoint is bounded even when a scan never responds and handles cancellation failure', async () => {
  await checkpointScans(['stuck', 'error'], id => id === 'error' ? Promise.reject(new Error('stopped')) : new Promise(() => {}), 10);
  let checkpointed = false;
  await prepareUpdateShutdown({ waitForUninstall: () => Promise.reject(new Error('uninstall failed')), checkpoint: async () => { checkpointed = true; } });
  assert.equal(checkpointed, true);
});
test('development updater is inert', async () => {
  const controller = createUpdateController({ updater: null, version: '0.5.0', enabled: false, prepareInstall: async () => assert.fail() });
  await controller.check(); await controller.download(); await controller.install();
  assert.equal(controller.snapshot().status, 'unavailable');
});

test('all accepted destructive RPC and filesystem operations settle before update checkpoint', async () => {
  const mutations = createMutationTracker();
  const execute = deferred(); const trash = deferred(); const restore = deferred();
  mutations.track(execute.promise); mutations.track(trash.promise); mutations.track(restore.promise);
  assert.equal(mutations.size, 3);
  let checkpointed = false;
  const shutdown = prepareUpdateShutdown({ waitForUninstall: () => mutations.settle(), checkpoint: async () => { checkpointed = true; } });
  execute.resolve(); trash.resolve(); await new Promise(setImmediate);
  assert.equal(checkpointed, false);
  restore.resolve(); await shutdown;
  assert.equal(checkpointed, true); assert.equal(mutations.size, 0);
});
test('native error cancels deferred install ownership; retry survives old preparation settling', async () => {
  const preparations = []; let locked = false; let quitPrepared = false;
  const updater = new EventEmitter(); updater.quitAndInstall = () => installs++;
  let installs = 0;
  const controller = createUpdateController({ updater, version: '0.5.0', enabled: true,
    onChange: state => { if (state.status === 'error') { locked = false; quitPrepared = false; } },
    prepareInstall: async ({ isCurrent }) => {
      locked = true;
      const gate = deferred(); preparations.push(gate);
      await prepareUpdateShutdown({ waitForUninstall: () => gate.promise, checkpoint: async () => {}, isCurrent });
      if (isCurrent()) quitPrepared = true;
    },
  });
  updater.emit('update-downloaded', { version: '0.6.0' });
  const abandoned = controller.install(); await new Promise(setImmediate);
  assert.equal(locked, true);
  updater.emit('error', new Error('native signature validation failed'));
  assert.equal(locked, false); assert.equal(quitPrepared, false);
  const retry = controller.install(); await new Promise(setImmediate);
  assert.equal(preparations.length, 2); assert.equal(locked, true);
  preparations[0].resolve(); await abandoned;
  assert.equal(installs, 0); assert.equal(quitPrepared, false); assert.equal(locked, true);
  // Old finally must not release the new command's owner.
  await controller.install(); assert.equal(preparations.length, 2);
  preparations[1].resolve(); await retry;
  assert.equal(installs, 1); assert.equal(quitPrepared, true);
});
test('abandoned shutdown never checkpoints scans admitted after its failure', async () => {
  const work = deferred(); let current = true; let checkpointed = false;
  const shutdown = prepareUpdateShutdown({ waitForUninstall: () => work.promise, isCurrent: () => current, checkpoint: async () => { checkpointed = true; } });
  current = false; work.resolve(); await shutdown;
  assert.equal(checkpointed, false);
});
