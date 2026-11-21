'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { createAppIconStore, extractIcnsPng, bundleIconNames, APP_ICON_LIMITS } = require('./app-icons.cjs');

const url = 'data:image/png;base64,aWNvbg==';
const image = (value = url) => ({ isEmpty: () => false, getSize: () => ({ width: 32, height: 32 }), toDataURL: () => value });
const pathFor = i => `/Applications/App${i}.app`;
const register = (store, count) => store.rememberApplications(Array.from({ length: count }, (_, i) => ({ path: pathFor(i) })));

test('only registered, absolute, canonical app paths reach Electron', async () => {
  const calls = [];
  const store = createAppIconStore({ app: { getFileIcon: async (...args) => { calls.push(args); return image(); } } });
  const invalid = [null, undefined, {}, { path: '/Applications/Unknown.app' }, { path: 'relative.app' },
    { path: '/Applications/../Secret.app' }, { path: '/Applications/./Secret.app' },
    { path: '/Applications//Secret.app' }, { path: '/Applications/Secret.app\0' }, { path: '/etc/passwd' }];
  store.rememberApplications(invalid.slice(4));
  for (const params of invalid) assert.equal(await store.getAppIcon(params), null);
  assert.equal(calls.length, 0);
  register(store, 1);
  assert.equal(await store.getAppIcon({ path: pathFor(0) }), url);
  assert.deepEqual(calls, [[pathFor(0), { size: 'normal' }]]);
});

test('parallel requests for one path share the same promise and cache entry', async () => {
  let calls = 0;
  let finish;
  const store = createAppIconStore({ app: { getFileIcon: () => { calls += 1; return new Promise(resolve => { finish = resolve; }); } } });
  register(store, 1);
  const first = store.getAppIcon({ path: pathFor(0) });
  const second = store.getAppIcon({ path: pathFor(0) });
  await Promise.resolve();
  assert.equal(first, second);
  assert.equal(calls, 1);
  finish(image());
  assert.deepEqual(await Promise.all([first, second]), [url, url]);
  assert.equal(await store.getAppIcon({ path: pathFor(0) }), url);
  assert.equal(calls, 1);
});

test('LRU evicts the least recently read entry at the entry bound', async () => {
  const calls = [];
  const store = createAppIconStore({ maxEntries: 2, app: { getFileIcon: async path => { calls.push(path); return image(); } } });
  register(store, 3);
  for (const i of [0, 1, 0, 2]) await store.getAppIcon({ path: pathFor(i) });
  assert.equal(store.getStats().entries, 2);
  await store.getAppIcon({ path: pathFor(0) });
  await store.getAppIcon({ path: pathFor(1) });
  assert.deepEqual(calls, [pathFor(0), pathFor(1), pathFor(2), pathFor(1)]);
});

test('UTF-16 payload and path byte accounting obeys the total memory bound', async () => {
  const store = createAppIconStore({ maxBytes: 200, app: { getFileIcon: async () => image() } });
  register(store, 20);
  for (let i = 0; i < 20; i += 1) {
    await store.getAppIcon({ path: pathFor(i) });
    assert.ok(store.getStats().bytes <= 200);
  }
  assert.equal(store.getStats().entries, 1);
});

test('default hard ceilings cannot be raised by configuration', async () => {
  const store = createAppIconStore({ maxEntries: 10000, maxBytes: 1e9, maxApplications: 1e9,
    app: { getFileIcon: async () => image() } });
  register(store, APP_ICON_LIMITS.maxApplications + 1);
  assert.equal(store.getStats().applications, APP_ICON_LIMITS.maxApplications);
  assert.equal(await store.getAppIcon({ path: pathFor(0) }), null);
  for (let i = 1; i <= 160; i += 1) await store.getAppIcon({ path: pathFor(i) });
  assert.equal(store.getStats().entries, APP_ICON_LIMITS.maxEntries);
  assert.ok(store.getStats().bytes <= APP_ICON_LIMITS.maxBytes);
});

test('registration eviction removes matching cached icons', async () => {
  const store = createAppIconStore({ maxApplications: 1, app: { getFileIcon: async () => image() } });
  register(store, 1);
  await store.getAppIcon({ path: pathFor(0) });
  store.rememberApplications([{ path: pathFor(1) }]);
  assert.deepEqual(store.getStats(), { entries: 0, bytes: 0, applications: 1, pending: 0, active: 0 });
  assert.equal(await store.getAppIcon({ path: pathFor(0) }), null);
});

test('only four lookups run concurrently and excess queued requests are rejected', async () => {
  const finishes = [];
  const store = createAppIconStore({ maxConcurrent: 99, maxPending: 5,
    app: { getFileIcon: () => new Promise(resolve => finishes.push(() => resolve(image()))) } });
  register(store, 6);
  const requests = Array.from({ length: 5 }, (_, i) => store.getAppIcon({ path: pathFor(i) }));
  await Promise.resolve();
  assert.equal(finishes.length, 4);
  assert.equal(store.getStats().active, 4);
  assert.equal(await store.getAppIcon({ path: pathFor(5) }), null);
  finishes[0]();
  await requests[0];
  await new Promise(setImmediate);
  assert.equal(finishes.length, 5);
  finishes.slice(1).forEach(finish => finish());
  assert.deepEqual(await Promise.all(requests), Array(5).fill(url));
  assert.equal(store.getStats().active, 0);
  assert.equal(store.getStats().pending, 0);
});

test('empty, oversized, malformed and rejected icons return bounded cached fallback', async () => {
  let encoded = 0;
  const variants = [
    { isEmpty: () => true },
    { isEmpty: () => false, getSize: () => ({ width: 5000, height: 5000 }), toDataURL: () => { encoded += 1; return url; } },
    image('data:image/png;base64,' + 'A'.repeat(100000)),
    image('https://unexpected.example/icon.png'),
    new Error('lookup failed'),
  ];
  let calls = 0;
  const store = createAppIconStore({ app: { getFileIcon: async path => {
    calls += 1;
    const value = variants[Number(path.match(/App(\d+)/)[1])];
    if (value instanceof Error) throw value;
    return value;
  } } });
  register(store, variants.length);
  for (let i = 0; i < variants.length; i += 1) {
    assert.equal(await store.getAppIcon({ path: pathFor(i) }), null);
    assert.equal(await store.getAppIcon({ path: pathFor(i) }), null);
  }
  assert.equal(encoded, 0);
  assert.equal(calls, variants.length);
  assert.ok(store.getStats().bytes <= APP_ICON_LIMITS.maxBytes);
});

test('an evicted registration cannot return an icon after its lookup finishes', async () => {
  let finish;
  const store = createAppIconStore({ maxApplications: 1,
    app: { getFileIcon: () => new Promise(resolve => { finish = resolve; }) } });
  register(store, 1);
  const request = store.getAppIcon({ path: pathFor(0) });
  await Promise.resolve();
  store.rememberApplications([{ path: pathFor(1) }]);
  finish(image());
  assert.equal(await request, null);
  assert.equal(store.getStats().entries, 0);
});

function pngHeader(side) {
  const bytes = Buffer.alloc(24);
  Buffer.from('89504e470d0a1a0a', 'hex').copy(bytes);
  bytes.write('IHDR', 12);
  bytes.writeUInt32BE(side, 16);
  bytes.writeUInt32BE(side, 20);
  return bytes;
}

function icns(...pngs) {
  const chunks = pngs.map(png => {
    const chunk = Buffer.alloc(8 + png.length);
    chunk.write('ic07'); chunk.writeUInt32BE(chunk.length, 4); png.copy(chunk, 8);
    return chunk;
  });
  const header = Buffer.alloc(8);
  header.write('icns'); header.writeUInt32BE(8 + chunks.reduce((sum, chunk) => sum + chunk.length, 0), 4);
  return Buffer.concat([header, ...chunks]);
}

test('ICNS extraction picks an appropriate embedded PNG without decoding oversized representations', () => {
  assert.equal(extractIcnsPng(icns(pngHeader(512), pngHeader(32), pngHeader(64))).readUInt32BE(16), 64);
  assert.equal(extractIcnsPng(icns(pngHeader(512), pngHeader(32), pngHeader(128))).readUInt32BE(16), 128);
  assert.equal(extractIcnsPng(icns(pngHeader(16), pngHeader(32))).readUInt32BE(16), 32);
  assert.equal(extractIcnsPng(icns(pngHeader(100000))), null);
  assert.equal(extractIcnsPng(Buffer.from('not an icon')), null);
  const broken = icns(pngHeader(64)); broken.writeUInt32BE(0, 12);
  assert.equal(extractIcnsPng(broken), null);
  assert.equal(extractIcnsPng(icns(pngHeader(64)).subarray(0, 20)), null);
});

test('bundle metadata accepts declared icon names and rejects path traversal', () => {
  assert.deepEqual(bundleIconNames({ CFBundleIconFile: 'AppIcon', CFBundleIcons: { CFBundlePrimaryIcon: {
    CFBundleIconFiles: ['AppIcon', 'AppIcon64.png', '../secret.png', '/secret.png', 'nested/../secret.png', 'bad\0.png'],
  } }, CFBundleIconName: 'OtherIcon' }), ['AppIcon', 'AppIcon64.png', 'OtherIcon']);
});

test('declared bundle ICNS is decoded from memory before system fallback; external symlinks are rejected',
  { skip: process.platform !== 'darwin' }, async t => {
    const fs = require('node:fs/promises');
    const os = require('node:os');
    const path = require('node:path');
    const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'app-icon-test-'));
    t.after(() => fs.rm(temporary, { recursive: true, force: true }));
    const application = path.join(temporary, 'Test.app');
    const resources = path.join(application, 'Contents/Resources');
    await fs.mkdir(resources, { recursive: true });
    await fs.writeFile(path.join(application, 'Contents/Info.plist'),
      '<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIconFile</key><string>AppIcon</string></dict></plist>');
    const resource = path.join(resources, 'AppIcon.icns');
    await fs.writeFile(resource, icns(pngHeader(64)));
    let systemCalls = 0;
    let decoded = 0;
    const nativeImage = { createFromBuffer(bytes) {
      decoded += 1;
      assert.equal(bytes.readUInt32BE(16), 64);
      return { isEmpty: () => false, resize: options => { assert.equal(options.width, 64); return image(); } };
    } };
    const create = () => {
      const store = createAppIconStore({ nativeImage, app: { getFileIcon: async () => { systemCalls += 1; return image(); } } });
      store.rememberApplications([{ path: application }]);
      return store;
    };
    assert.equal(await create().getAppIcon({ path: application }), url);
    assert.equal(decoded, 1);
    assert.equal(systemCalls, 0);
    await fs.unlink(resource);
    const outside = path.join(temporary, 'outside.icns');
    await fs.writeFile(outside, icns(pngHeader(64)));
    await fs.symlink(outside, resource);
    assert.equal(await create().getAppIcon({ path: application }), url);
    assert.equal(decoded, 1);
    assert.equal(systemCalls, 1);
    await fs.unlink(resource);
    const large = await fs.open(resource, 'w');
    await large.truncate(8 * 1024 * 1024 + 1);
    await large.close();
    assert.equal(await create().getAppIcon({ path: application }), url);
    assert.equal(decoded, 1);
    assert.equal(systemCalls, 2);
    await fs.rm(resources, { recursive: true });
    await fs.symlink(temporary, resources);
    assert.equal(await create().getAppIcon({ path: application }), url);
    assert.equal(decoded, 1);
    assert.equal(systemCalls, 3);
  });
