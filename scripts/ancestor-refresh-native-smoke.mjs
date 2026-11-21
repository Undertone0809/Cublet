// Native Electron + Rust regression; only owned synthetic fixture files are changed.
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { access, link, lstat, mkdir, mkdtemp, open, realpath, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir, userInfo } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin');
const project = fileURLToPath(new URL('..', import.meta.url));
const core = resolve(process.env.CODEX_CORE_PATH ?? join(project, 'rust-core/target/debug/codex-core'));
await access(core);
await access(join(project, 'dist/index.html'));
const applications = join(await realpath(userInfo().homedir), 'Applications');
const root = await mkdtemp(join(await realpath(tmpdir()), 'msm-ancestor-smoke-'));
const fixture = await mkdtemp(join(applications, 'msm-ancestor-smoke-'));
const parent = join(fixture, 'Parent');
const child = join(parent, 'Child');
const sibling = join(fixture, 'Sibling');
const payload = join(child, 'delete-me.bin');
const secondPayload = join(child, 'delete-next.bin');
const bootstrap = join(root, 'main.cjs');
const env = { ...process.env, CODEX_CORE_PATH: core, CODEX_HOME: join(root, 'codex'), MSM_DISK_CACHE_ROOT: join(root, 'cache') };
for (const key of ['ELECTRON_START_URL', 'ELECTRON_REMOTE_DEBUGGING_PORT', 'ELECTRON_RUN_AS_NODE']) delete env[key];
const errors = [];
let app;
let page;
let step = 'creating fixtures';
const MiB = 1024 * 1024;
const formatBytes = bytes => {
  if (!bytes) return '0 B';
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1000)), 4);
  return `${(bytes / 1000 ** index).toFixed(index ? 1 : 0)} ${['B', 'KB', 'MB', 'GB', 'TB'][index]}`;
};
const invoke = (method, params = {}) => page.evaluate(({ method, params }) => window.electronAPI.invoke(method, params), { method, params });
const tile = path => page.locator(`[data-directory-tile=${JSON.stringify(path)}]`).getByRole('button').first();
const location = () => page.getByRole('navigation', { name: 'Current path', exact: true });
const status = () => page.locator('.analysis-status');

async function allocate(path, mebibytes) {
  const file = await open(path, 'w');
  try {
    for (let i = 0; i < mebibytes; i++) await file.writeFile(randomBytes(MiB));
    await file.sync();
  } finally { await file.close(); }
}
async function until(read, description) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const result = await read();
    if (result) return result;
    await delay(100);
  }
  throw new Error(`Timed out: ${description}`);
}
async function cached(path, previousId) {
  return until(async () => {
    const result = (await invoke('get_cached_disk_scan', { path, allowProtected: false }))
      ?? (await invoke('get_cached_disk_scan', { path, allowProtected: true }));
    return result?.status === 'complete' && result.scanId !== previousId ? result : null;
  }, `completed exact scan for ${basename(path)}`);
}
async function displayed(path, scan) {
  await location().locator('[aria-current="location"]').filter({ hasText: basename(path) }).waitFor();
  await until(async () => {
    const text = await status().innerText();
    return text.includes(`${formatBytes(scan.allocatedBytes)} · ${scan.scannedFiles.toLocaleString()} files`) &&
      !text.includes('updating in background') && !text.includes('Measuring disk usage');
  }, `displayed total for ${basename(path)}`);
}
async function enter(path) {
  await tile(path).press('Enter');
  await location().locator('[aria-current="location"]').filter({ hasText: basename(path) }).waitFor();
  await until(async () => (await page.getByRole('button', { name: 'Rescan', exact: true }).isEnabled()), 'navigation settled');
}
async function jump(path) {
  await location().getByRole('button', { name: basename(path), exact: true }).click();
}
async function launch() {
  app = await _electron.launch({ args: [bootstrap], cwd: project, env });
  page = await app.firstWindow();
  page.setDefaultTimeout(20_000);
  page.on('pageerror', error => errors.push(String(error)));
  await page.addInitScript(() => {
    localStorage.setItem('codex-storage-manager.locale.v1', 'en');
    localStorage.setItem('mac-storage-manager.permission-choice.v1', 'limited');
  });
  await page.reload();
  await page.getByRole('button', { name: 'Rescan', exact: true }).waitFor();
  await page.getByRole('combobox', { name: 'Scan scope' }).click();
  await page.getByRole('option', { name: 'User Applications', exact: true }).click();
}
async function freshReference(path) {
  // A separate Rust process/cache proves totals without touching renderer state.
  const { spawn } = await import('node:child_process');
  const { createInterface } = await import('node:readline');
  const process = spawn(core, [], { env: { ...env, MSM_DISK_CACHE_ROOT: join(root, 'reference-cache') }, stdio: ['pipe', 'pipe', 'pipe'] });
  const lines = createInterface({ input: process.stdout });
  const pending = new Map();
  let id = 0;
  lines.on('line', line => {
    const response = JSON.parse(line);
    const request = pending.get(response.id);
    if (!request) return;
    pending.delete(response.id);
    if (response.ok) request.resolve(response.result);
    else request.reject(new Error(response.error));
  });
  const rpc = (method, params) => new Promise((resolve, reject) => {
    const requestId = String(++id);
    pending.set(requestId, { resolve, reject });
    process.stdin.write(`${JSON.stringify({ id: requestId, method, params })}\n`);
  });
  try {
    const scan = await rpc('start_disk_scan', { path, refresh: true, allowProtected: false });
    return await until(async () => {
      const result = await rpc('get_disk_scan', { scanId: scan.scanId });
      assert.notEqual(result.status, 'failed');
      return result.status === 'complete' ? result : null;
    }, 'independent native reference');
  } finally { process.kill(); lines.close(); }
}
async function assertReference(scan, path) {
  const reference = await freshReference(path);
  for (const field of ['allocatedBytes', 'logicalBytes', 'scannedFiles', 'scannedDirectories']) {
    assert.equal(scan[field], reference[field], `${basename(path)} ${field} must match an independent scan`);
  }
}

try {
  await Promise.all([mkdir(child, { recursive: true }), mkdir(sibling), mkdir(join(root, 'user-data')), mkdir(join(root, 'codex'))]);
  await writeFile(bootstrap, `
    const { app } = require('electron');
    app.setPath('appData', ${JSON.stringify(root)});
    app.setPath('userData', ${JSON.stringify(join(root, 'user-data'))});
    require(${JSON.stringify(join(project, 'electron/main.cjs'))});
  `);
  await Promise.all([allocate(payload, 4), allocate(secondPayload, 2), allocate(join(child, 'shared.bin'), 1), allocate(join(sibling, 'untouched.bin'), 1)]);
  await link(join(child, 'shared.bin'), join(parent, 'shared-link.bin'));
  step = 'opening the measured native tree';
  await launch();
  const original = await cached(applications);
  await enter(fixture);
  const oldFixture = await invoke('get_disk_scan_view', { scanId: original.scanId, path: fixture });
  await enter(parent);
  await enter(child);

  step = 'deleting files, rescanning a child and going back two levels';
  await rm(payload);
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  const firstChild = await cached(child);
  await displayed(child, firstChild);
  assert.equal(firstChild.scannedFiles, 2);
  await page.keyboard.press('Alt+ArrowLeft');
  const firstParent = await cached(parent);
  await displayed(parent, firstParent);
  await assertReference(firstParent, parent);
  await page.keyboard.press('Alt+ArrowLeft');
  const firstFixture = await cached(fixture);
  await displayed(fixture, firstFixture);
  await assertReference(firstFixture, fixture);
  assert.equal(oldFixture.allocatedBytes - firstFixture.allocatedBytes, 4 * MiB);
  assert.equal(firstFixture.scannedFiles, oldFixture.scannedFiles - 1);
  console.log('PASS child rescan → Back twice; allocated bytes and file counts match fresh native scans, including cross-boundary hardlinks');

  step = 'reusing updated ancestors without redundant scans';
  await enter(parent);
  await enter(child);
  await jump(fixture);
  await displayed(fixture, firstFixture);
  assert.equal((await cached(fixture)).scanId, firstFixture.scanId);
  console.log('PASS repeated ancestor navigation reuses the updated measurement');

  step = 'direct breadcrumb jump after another child rescan';
  await enter(parent);
  await enter(child);
  await rm(secondPayload);
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  const secondChild = await cached(child, firstChild.scanId);
  await displayed(child, secondChild);
  await jump(fixture);
  const secondFixture = await cached(fixture, firstFixture.scanId);
  await displayed(fixture, secondFixture);
  await assertReference(secondFixture, fixture);
  assert.equal(firstFixture.allocatedBytes - secondFixture.allocatedBytes, 2 * MiB);
  await page.screenshot({ path: join(root, 'ancestor-refreshed.png') });
  console.log('PASS breadcrumb jump refreshes the selected ancestor');

  step = 'retaining invalidation after a native relaunch';
  // The broad User Applications cache predates both deletions. The persisted
  // renderer ledger must prevent restoring it after its resident source is gone.
  await app.close(); app = undefined;
  await launch();
  const restored = await cached(applications, original.scanId);
  await displayed(applications, restored);
  const restoredFixture = await invoke('get_disk_scan_view', { scanId: restored.scanId, path: fixture });
  assert.equal(restoredFixture.scannedFiles, secondFixture.scannedFiles);
  assert.equal(restoredFixture.allocatedBytes, secondFixture.allocatedBytes);
  assert.equal(await page.getByRole('alert').count(), 0);
  assert.deepEqual(errors, []);
  console.log('PASS stale persisted ancestor cache is remeasured after relaunch');
  console.log(`Native evidence: ${root}`);
} catch (error) {
  if (page) await page.screenshot({ path: join(root, 'failure.png') }).catch(() => {});
  console.error(`FAIL ${step}; native evidence: ${root}`);
  throw error;
} finally {
  if (app) await app.close();
  await rm(fixture, { recursive: true, force: true });
}
