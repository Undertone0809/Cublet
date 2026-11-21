// Run after npm run build and npm run electron:build:core on macOS.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { constants } from 'node:fs';
import { access, copyFile, lstat, mkdir, mkdtemp, readFile, realpath, rename, rm, utimes, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir, userInfo } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin', 'This test requires native macOS Trash and process inspection');
const project = fileURLToPath(new URL('..', import.meta.url));
const core = resolve(process.env.CODEX_CORE_PATH ?? join(project, 'rust-core/target/debug/codex-core'));
const electron = createRequire(import.meta.url)('electron');
await access(core, constants.X_OK);
await access(join(project, 'dist/index.html'));
await access(electron, constants.X_OK);
await access(join(dirname(electron), '..', 'Frameworks', 'Electron Framework.framework', 'Electron Framework'));

const home = await realpath(userInfo().homedir);
const applications = join(home, 'Applications');
const id = randomUUID();
const name = `Storage Manager Smoke ${id}`;
const bundleId = `com.example.storage-manager-smoke.${id}`;
const dataPath = join(home, 'Library', 'Application Support', bundleId);
const trashPath = join(home, '.Trash', `${name}.app`);
assert((await lstat(applications)).isDirectory(), 'Create ~/Applications before running this test');
await assert.rejects(lstat(trashPath), { code: 'ENOENT' });
const root = await mkdtemp(join(await realpath(tmpdir()), 'msm-native-smoke-'));
const cacheRoot = join(root, 'cache');
const userData = join(root, 'user-data');
const codexHome = join(root, 'codex');
const bootstrap = join(root, 'main.cjs');
const errors = [];
let app;
let activePage;
let running;
let fixtureFolder;
let dataCreated = false;
let bundleIdentity;
let step = 'verifying an isolated cache';

async function makeBundle(bundle) {
  const executable = join(bundle, 'Contents', 'MacOS', 'fixture-sleep');
  await mkdir(join(bundle, 'Contents', 'MacOS'), { recursive: true });
  await copyFile('/bin/sleep', executable);
  await writeFile(join(bundle, 'Contents', 'Info.plist'), `<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>${bundleId}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleExecutable</key><string>fixture-sleep</string>
</dict></plist>`);
  return executable;
}

function readCacheSettings(env) {
  const { stdout, stderr, status, error } = spawnSync(core, [], {
    env, timeout: 5000, encoding: 'utf8',
    input: `${JSON.stringify({ id: 'smoke-cache', method: 'get_disk_cache_settings', params: {} })}\n`,
  });
  assert.ifError(error);
  assert.equal(status, 0, stderr);
  const response = JSON.parse(stdout);
  assert(response.ok, response.error);
  return response.result;
}

async function launch(env) {
  app = await _electron.launch({ args: [bootstrap], cwd: project, env });
  const page = await app.firstWindow();
  activePage = page;
  page.setDefaultTimeout(15_000);
  page.on('pageerror', (error) => errors.push(String(error)));
  await page.addInitScript(() => {
    localStorage.setItem('codex-storage-manager.locale.v1', 'en');
    localStorage.setItem('mac-storage-manager.permission-choice.v1', 'limited');
  });
  await page.reload();
  // This regression intentionally exercises native Trash. The product default
  // is permanent deletion; this debug sidecar persists settings under cacheRoot.
  const policy = await page.evaluate(() => window.electronAPI.invoke('set_cleanup_disposition', { mode: 'trash' }));
  assert.equal(policy.mode, 'trash');
  await page.getByRole('navigation').getByRole('button', { name: 'Applications', exact: true }).click();
  assert.equal(await page.getByRole('combobox', { name: 'Scan scope' }).count(), 0,
    'Application discovery must be independent of the Space scan scope');
  return page;
}

async function seedTemporaryRecovery(operationId, logNumber) {
  const id = `log/codex-tui.log.${logNumber}`;
  const source = join(codexHome, id);
  const recovery = join(codexHome, 'recovery', 'codex-storage-manager', operationId);
  const saved = join(recovery, 'live', id);
  await mkdir(dirname(source), { recursive: true });
  await mkdir(dirname(saved), { recursive: true });
  await writeFile(saved, operationId);
  await utimes(saved, 1_000_000, 1_000_000);
  const stat = await lstat(saved, { bigint: true });
  const bytes = Number(stat.blocks) * 512;
  const stamp = { relative: '', bytes: Number(stat.size), modified_ns: Number(stat.mtimeNs), inode: Number(stat.ino), device: Number(stat.dev), is_dir: false };
  const entry = { id, path: source, kind: 'rotated_log', bytes, file_count: 1, can_remove: true, blocking_reason: null };
  const plan = { plan_id: operationId, codex_home: codexHome, created_at: `2026-01-0${logNumber}T00:00:00Z`, entries: [entry], estimated_bytes: bytes };
  await writeFile(join(recovery, 'live-operation.json'), JSON.stringify({ plan, stamps: { [id]: [stamp] }, status: 'completed', moved: [id] }));
  return { source, saved };
}

async function checkTemporaryRecovery(page) {
  const restore = await seedTemporaryRecovery('live-smoke-restore', 1);
  const purge = await seedTemporaryRecovery('live-smoke-purge', 2);
  assert.deepEqual(await sidebar(page).getByRole('button').allTextContents(), ['Space explorer', 'Applications', 'Codex tools', 'AI connections', 'Settings']);
  await sidebar(page).getByRole('button', { name: 'Codex tools' }).click();
  await page.getByRole('navigation', { name: 'Codex tools navigation' }).getByRole('button', { name: 'Recovery & history' }).click();
  const rows = page.locator('.operation-row').filter({ hasText: '1 temporary files' });
  await rows.nth(1).getByRole('button', { name: 'Restore', exact: true }).click();
  await rows.nth(1).getByText(/Restored/).waitFor();
  assert.equal(await readFile(restore.source, 'utf8'), 'live-smoke-restore');
  await rows.first().getByRole('button', { name: 'Clear recovery copies' }).click();
  const dialog = page.getByRole('dialog');
  const confirm = dialog.getByRole('button', { name: 'Permanently clear recovery' });
  assert(await confirm.isDisabled());
  await dialog.getByRole('checkbox').check();
  await confirm.click();
  await page.getByText('Temporary file recovery copies permanently deleted.').waitFor();
  await assert.rejects(lstat(purge.saved), { code: 'ENOENT' });
  await assert.rejects(lstat(purge.source), { code: 'ENOENT' });
  console.log('PASS Quick cleanup removed and legacy temporary file recovery preserved');
}

async function cachedScan(page, scope = applications) {
  return page.evaluate(async (path) => {
    const lookup = (allowProtected) => window.electronAPI.invoke('get_cached_disk_scan', { path, allowProtected });
    return (await lookup(false)) ?? (await lookup(true));
  }, scope);
}

async function scanFixtureScope(page, path) {
  // Applications no longer starts or changes a Space scan. Establish the exact
  // persisted scope explicitly, allowing an unrelated startup scan to finish.
  let scan;
  for (let attempt = 0; ; attempt += 1) {
    try {
      scan = await page.evaluate((scope) => window.electronAPI.invoke('start_disk_scan', { path: scope, allowProtected: true }), path);
      break;
    } catch (error) {
      if (!String(error).includes('A disk scan is already running') || attempt >= 300) throw error;
      await delay(100);
    }
  }
  for (let attempt = 0; attempt < 750; attempt += 1) {
    const current = await page.evaluate(scanId => window.electronAPI.invoke('get_disk_scan', { scanId }), scan.scanId);
    if (current.status !== 'running') return current;
    await delay(40);
  }
  throw new Error('Fixture scan did not finish');
}

const list = (page) => page.getByRole('table', { name: 'Applications', exact: true });
const appRow = (page) => list(page).getByRole('row').filter({ hasText: name });
const itemFor = (dialog, target) => dialog.getByRole('listitem').filter({ has: dialog.page().getByText(target, { exact: true }) });
const moveButton = (dialog) => dialog.getByRole('button', { name: 'Move to Trash', exact: true });
const sidebar = (page) => page.getByRole('navigation', { name: 'Main navigation' });

async function review(page, bundle) {
  const checkbox = appRow(page).getByRole('checkbox', { name: `Select ${name} · ${bundle}`, exact: true });
  await checkbox.waitFor();
  assert(await checkbox.isEnabled(), 'The installed bundle must be selectable by its exact path');
  if (!(await checkbox.isChecked())) await checkbox.click();
  await page.getByRole('button', { name: 'Review uninstall', exact: true }).click();
  const dialog = page.getByRole('dialog');
  await itemFor(dialog, bundle).waitFor();
  return dialog;
}

async function closeDialog(dialog, button) {
  await dialog.getByRole('button', { name: button, exact: true }).click();
  await dialog.waitFor({ state: 'hidden' });
}

async function stopFixture() {
  if (running && running.exitCode === null && running.signalCode === null) {
    const stopped = once(running, 'exit');
    running.kill();
    await stopped;
  }
  running = undefined;
}

async function checkExplorer(page) {
  const projectPath = join(fixtureFolder, 'Project');
  const leaf = join(projectPath, 'Leaf');
  await mkdir(leaf, { recursive: true });
  await writeFile(join(projectPath, 'large-sibling.bin'), Buffer.alloc(2 * 1024 * 1024, 0x5a));
  for (let start = 0; start < 220; start += 20) {
    await Promise.all(Array.from({ length: 20 }, (_, i) => {
      const name = `sample-${String(start + i).padStart(3, '0')}.bin`;
      return writeFile(join(leaf, name), Buffer.alloc(4096, 0x33));
    }));
  }
  await sidebar(page).getByRole('button', { name: 'Space explorer', exact: true }).click();
  const scope = page.getByRole('combobox', { name: 'Scan scope' });
  await scope.click();
  await page.getByRole('option', { name: 'User Applications', exact: true }).click();
  await page.getByRole('button', { name: 'Stop scan', exact: true }).waitFor({ state: 'hidden' });
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  await page.getByRole('button', { name: 'Stop scan', exact: true }).waitFor({ state: 'hidden' });
  await page.getByRole('button', { name: 'List', exact: true }).click();
  await page.getByRole('button', { name: basename(fixtureFolder), exact: true }).click();
  await page.getByRole('button', { name: 'Project', exact: true }).click();
  await page.getByRole('button', { name: 'Map', exact: true }).click();
  const breadcrumbs = page.getByRole('navigation', { name: 'Current path' });
  const current = breadcrumbs.locator('[aria-current="location"]');
  const tile = (path) => page.locator(`[data-directory-tile=${JSON.stringify(path)}]`).getByRole('button');
  await current.filter({ hasText: /^Project$/ }).waitFor();
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  await page.getByRole('button', { name: /^large-sibling\.bin,/ }).waitFor();
  await page.getByRole('button', { name: 'Stop scan', exact: true }).waitFor({ state: 'hidden' });
  await tile(leaf).press('Enter');
  await current.filter({ hasText: /^Leaf$/ }).waitFor();
  await page.getByText(/^Map shows .+ · .+ not shown$/).waitFor();
  assert.equal(await tile(leaf).count(), 0, 'The current folder must not be drawn as its own tile');
  assert.equal(await page.getByRole('button', { name: /^Other contents,/ }).count(), 0);
  await page.getByRole('button', { name: 'Directory details', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Leaf' });
  await dialog.getByText('Page 1 · 200 items', { exact: true }).waitFor();
  await dialog.getByRole('button', { name: 'Next', exact: true }).click();
  await dialog.getByText('Page 2 · 20 items', { exact: true }).waitFor();
  await closeDialog(dialog, 'Close');
  const ancestor = breadcrumbs.getByRole('button', { name: 'Project', exact: true });
  await ancestor.focus();
  assert(await ancestor.evaluate((element) => element === document.activeElement));
  await page.keyboard.press('Enter');
  await current.filter({ hasText: /^Project$/ }).waitFor();
  await page.waitForFunction(() => document.activeElement?.getAttribute('aria-current') === 'location');
  await tile(leaf).waitFor();
  console.log('PASS Native nested path, coverage without a current-folder tile, live paginated details, and ancestor navigation');
}

async function checkWindowAppearance(page) {
  const nativeWindow = await app.browserWindow(page);
  assert.deepEqual(await nativeWindow.evaluate((handle) => handle.getMinimumSize()), [900, 620]);
  await nativeWindow.evaluate((handle) => handle.setSize(900, 620));
  assert.deepEqual(await nativeWindow.evaluate((handle) => handle.getSize()), [900, 620]);
  for (const theme of ['dark', 'light']) {
    await sidebar(page).getByRole('button', { name: 'Settings', exact: true }).click();
    const label = theme === 'dark' ? 'Dark' : 'Light';
    await page.getByRole('group', { name: 'Appearance' }).getByRole('button', { name: label, exact: true }).click();
    await page.locator(`html.mac-native-window[data-theme="${theme}"]`).waitFor({ state: 'attached' });
    let actual = await app.evaluate(({ nativeTheme }) => nativeTheme.themeSource);
    for (let attempt = 0; actual !== theme && attempt < 20; attempt += 1) {
      await delay(50);
      actual = await app.evaluate(({ nativeTheme }) => nativeTheme.themeSource);
    }
    assert.equal(actual, theme);
    await sidebar(page).getByRole('button', { name: 'Space explorer', exact: true }).click();
    await page.getByRole('navigation', { name: 'Current path' }).waitFor();
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
  }
  console.log('PASS Native light and dark appearance at 900×620');
}

try {
  await mkdir(userData);
  await mkdir(codexHome);
  const cacheSettings = join(cacheRoot, 'com.zeeland.codex-storage-manager', 'disk-cache-limit');
  await mkdir(join(cacheRoot, 'com.zeeland.codex-storage-manager'), { recursive: true });
  await writeFile(bootstrap, `
    const { app } = require('electron');
    app.setPath('appData', ${JSON.stringify(root)});
    app.setPath('userData', ${JSON.stringify(userData)});
    require(${JSON.stringify(join(project, 'electron/main.cjs'))});
  `);
  const env = { ...process.env, CODEX_CORE_PATH: core, CODEX_HOME: codexHome, MSM_DISK_CACHE_ROOT: cacheRoot };
  delete env.ELECTRON_START_URL;
  delete env.ELECTRON_REMOTE_DEBUGGING_PORT;
  delete env.ELECTRON_RUN_AS_NODE;
  for (const limit of [64, 256]) {
    await writeFile(cacheSettings, `${limit}\n`);
    assert.deepEqual(readCacheSettings(env), { limitMiB: limit, usedBytes: 0 }, 'Build a debug sidecar that honors MSM_DISK_CACHE_ROOT');
  }

  step = 'scanning and selecting an installed fixture';
  fixtureFolder = await mkdtemp(join(applications, 'msm-native-smoke-'));
  const bundle = join(fixtureFolder, `${name}.app`);
  const executable = await makeBundle(bundle);
  await mkdir(dataPath);
  dataCreated = true;
  const dataFile = join(dataPath, 'untouched.txt');
  await writeFile(dataFile, id);

  const page = await launch(env);
  await page.getByRole('textbox', { name: 'Search applications by name or path' }).fill(name);
  await appRow(page).waitFor();
  const measured = await scanFixtureScope(page, applications);
  assert.equal(measured.status, 'complete');
  const before = await cachedScan(page);
  assert.equal(before?.status, 'complete');
  assert(before.applications.some((application) => application.path === bundle));

  step = 'blocking a running executable';
  running = spawn(executable, ['60'], { stdio: 'ignore' });
  await once(running, 'spawn');
  let dialog = await review(page, bundle);
  assert(await moveButton(dialog).isDisabled(), 'A running application must be blocked');
  assert.match(await itemFor(dialog, bundle).innerText(), /running|in use|is using|正在使用/i);
  assert(await itemFor(dialog, bundle).getByRole('checkbox').isDisabled());
  const recheck = dialog.getByRole('button', { name: 'Check again', exact: true });
  assert(await recheck.isEnabled(), 'Blocked applications need an actionable recheck');
  await stopFixture();
  await recheck.click();
  await itemFor(dialog, bundle).getByRole('checkbox').waitFor();
  assert(await itemFor(dialog, bundle).getByRole('checkbox').isEnabled());
  assert(await itemFor(dialog, bundle).getByRole('checkbox').isChecked());
  assert(await moveButton(dialog).isEnabled(), 'A new validated plan must recover after the fixture stops');
  assert.equal(await itemFor(dialog, dataPath).getByRole('checkbox').isChecked(), false);
  await itemFor(dialog, dataPath).getByRole('checkbox').click();
  await recheck.click();
  await itemFor(dialog, dataPath).getByRole('checkbox').waitFor();
  assert(await itemFor(dialog, dataPath).getByRole('checkbox').isChecked(), 'Recheck must preserve eligible optional choices using fresh plan IDs');
  await closeDialog(dialog, 'Cancel');

  step = 'protecting shared data and filtering a Library copy';
  const backups = join(dataPath, 'Backups');
  const outsideBundle = join(backups, `${name}.app`);
  await makeBundle(outsideBundle);
  const outsideScan = await scanFixtureScope(page, backups);
  assert.equal(outsideScan.status, 'complete');
  assert(outsideScan.entries.some((entry) => entry.path === outsideBundle), 'The space scan must retain the Library copy');
  assert(!outsideScan.applications.some((application) => application.path === outsideBundle), 'The Applications inventory must exclude the Library copy');
  const outsidePreview = await page.evaluate((path) => window.electronAPI.invoke('preview_app_uninstall', { paths: [path] }), outsideBundle);
  assert.equal(outsidePreview.items.length, 1);
  assert.equal(outsidePreview.items[0].path, outsideBundle);
  assert(outsidePreview.items[0].blockedReason, 'A copy under Library must be ineligible for uninstall');
  dialog = await review(page, bundle);
  assert(await itemFor(dialog, bundle).getByRole('checkbox').isChecked());
  const sharedData = itemFor(dialog, dataPath);
  await sharedData.waitFor();
  assert(await sharedData.getByRole('checkbox').isDisabled(), 'A matching copy must protect shared data');
  assert.match(await sharedData.innerText(), /same bundle|same identifier|同一标识/i);
  await closeDialog(dialog, 'Cancel');
  await access(outsideBundle);
  await rm(backups, { recursive: true });

  step = 'rejecting a bundle replaced after preview';
  dialog = await review(page, bundle);
  assert(await itemFor(dialog, bundle).getByRole('checkbox').isChecked());
  const dataItem = itemFor(dialog, dataPath);
  await dataItem.waitFor();
  assert.equal(await dataItem.getByRole('checkbox').isChecked(), false, 'Optional data must start unchecked');
  await rename(bundle, join(fixtureFolder, 'original'));
  await makeBundle(bundle);
  await moveButton(dialog).click();
  await dialog.getByText('Moved to Trash: 0', { exact: true }).waitFor();
  assert.match(await dialog.getByRole('list', { name: 'Unfinished items' }).innerText(), /changed|发生变化/i);
  await access(bundle);
  await assert.rejects(lstat(trashPath), { code: 'ENOENT' });
  await closeDialog(dialog, 'Done');

  step = 'moving the exact bundle to native Trash and preserving its data';
  dialog = await review(page, bundle);
  assert(await itemFor(dialog, bundle).getByRole('checkbox').isChecked());
  assert.equal(await itemFor(dialog, dataPath).getByRole('checkbox').isChecked(), false);
  bundleIdentity = await lstat(bundle, { bigint: true });
  await moveButton(dialog).click();
  await dialog.getByText('Moved to Trash: 1', { exact: true }).waitFor();
  await dialog.getByText('Show original paths of completed items', { exact: true }).click();
  await dialog.getByText(bundle, { exact: true }).waitFor();
  await assert.rejects(lstat(bundle), { code: 'ENOENT' });
  const trashed = await lstat(trashPath, { bigint: true });
  assert.deepEqual([trashed.dev, trashed.ino], [bundleIdentity.dev, bundleIdentity.ino]);
  assert.equal(await readFile(dataFile, 'utf8'), id);
  await closeDialog(dialog, 'Done');
  await appRow(page).waitFor({ state: 'hidden' });
  // Discovery owns the application page; stale Space measurements are checked
  // through their persisted snapshot rather than an Applications status label.
  assert.equal((await cachedScan(page)).usageStale, true);
  await app.close();
  app = undefined;

  step = 'reopening the persisted scan after restart';
  const restarted = await launch(env);
  const cached = await cachedScan(restarted);
  assert.equal(cached?.cacheHit, true, 'The restarted sidecar must read the original persisted scan');
  assert.equal(cached.startedAtMs, before.startedAtMs);
  assert.equal(cached.usageStale, true);
  assert(!cached.applications.some((application) => application.path === bundle));
  await appRow(restarted).waitFor({ state: 'hidden' });
  await rename(trashPath, bundle);
  const restored = await cachedScan(restarted);
  assert(restored.applications.some((application) => application.path === bundle), 'Restoring the exact path must make the bundle visible in the cached inventory');
  console.log('PASS Applications selection, running app protection, replacement protection, native Trash, unchecked data, cached inventory after restart and restore, and Library copy protection');

  step = 'drilling through nested native directories';
  await checkExplorer(restarted);
  step = 'checking the minimum window in light and dark appearance';
  await checkWindowAppearance(restarted);
  step = 'recovering older temporary files from Codex history';
  await checkTemporaryRecovery(restarted);
  assert.deepEqual(errors, []);
} catch (error) {
  if (fixtureFolder) {
    const usage = spawnSync('/usr/sbin/lsof', ['-nP', '+D', join(fixtureFolder, `${name}.app`)], { encoding: 'utf8', timeout: 6000 });
    console.error('Owned fixture open-file evidence:', usage.stdout || usage.stderr || `(exit ${usage.status})`);
  }
  const dialog = activePage?.getByRole('dialog');
  if (dialog && await dialog.isVisible().catch(() => false)) {
    console.error((await dialog.innerText().catch(() => '')).slice(0, 1200));
  }
  const alerts = await activePage?.getByRole('alert').allTextContents().catch(() => []);
  if (alerts?.length) console.error(alerts.map((alert) => alert.slice(0, 500)).join('\n'));
  throw new Error(`Native smoke failed while ${step}`, { cause: error });
} finally {
  try {
    await stopFixture();
    await app?.close();
  } finally {
    if (bundleIdentity) {
      const trashed = await lstat(trashPath, { bigint: true }).catch((error) => {
        if (error.code !== 'ENOENT') throw error;
      });
      if (trashed?.dev === bundleIdentity.dev && trashed.ino === bundleIdentity.ino) {
        await rm(trashPath, { recursive: true });
      }
    }
    if (fixtureFolder) await rm(fixtureFolder, { recursive: true, force: true });
    if (dataCreated) await rm(dataPath, { recursive: true, force: true });
    await rm(root, { recursive: true, force: true });
  }
}
