// Real Electron IPC, Rust measurements and macOS Trash; changes only owned fixtures.
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { access, link, lstat, mkdir, mkdtemp, readFile, readlink, realpath, rename, rm, symlink, writeFile } from 'node:fs/promises';
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
const home = await realpath(userInfo().homedir);
const applications = join(home, 'Applications');
const evidence = await mkdtemp(join(await realpath(tmpdir()), 'msm-selection-trash-'));
const fixture = await mkdtemp(join(applications, 'msm-selection-trash-'));
const parent = join(fixture, 'Parent');
const target = join(parent, `Trash-this-folder-${basename(fixture)}`);
const retained = join(parent, 'Retained');
const missing = join(parent, 'Missing-dates');
const replacement = join(fixture, 'Replacement-protection');
const savedOriginal = join(fixture, 'Replacement-original');
const trashed = join(home, '.Trash', basename(target));
const payload = randomBytes(2 * 1024 * 1024);
const bootstrap = join(evidence, 'main.cjs');
const env = { ...process.env, CODEX_CORE_PATH: core, CODEX_HOME: join(evidence, 'codex'), MSM_DISK_CACHE_ROOT: join(evidence, 'cache') };
for (const key of ['ELECTRON_START_URL', 'ELECTRON_REMOTE_DEBUGGING_PORT', 'ELECTRON_RUN_AS_NODE']) delete env[key];
const errors = [];
let app;
let page;
let step = 'creating owned fixtures';
let movedIdentity;
let libraryFixture;
const additionalTrash = [];
const nativeCases = [];
const invoke = (method, params = {}) => page.evaluate(({ method, params }) => window.electronAPI.invoke(method, params), { method, params });
const tile = path => page.locator(`[data-directory-tile=${JSON.stringify(path)}]`).getByRole('button').first();
const panel = () => page.getByRole('complementary', { name: 'Selected item', exact: true });
const dialog = () => page.getByRole('dialog', { name: 'Move this item to Trash?', exact: true });
const moveButton = () => panel().getByRole('button', { name: 'Move to Trash', exact: true });
const location = () => page.getByRole('navigation', { name: 'Current path', exact: true });

async function absent(path) {
  try { await lstat(path); return false; }
  catch (error) { if (error.code === 'ENOENT') return true; throw error; }
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
async function cached(path, previousId, startedAtLeast = 0) {
  return until(async () => {
    const result = (await invoke('get_cached_disk_scan', { path, allowProtected: false }))
      ?? (await invoke('get_cached_disk_scan', { path, allowProtected: true }));
    assert.notEqual(result?.status, 'failed');
    return result?.status === 'complete' && result.scanId !== previousId && result.startedAtMs >= startedAtLeast ? result : null;
  }, `completed exact native scan for ${basename(path)}`);
}
async function settled() {
  await until(async () => await page.getByRole('button', { name: 'Rescan', exact: true }).isEnabled()
    && !(await page.locator('.analysis-status').innerText()).match(/updating in background|Measuring disk usage/), 'navigation and scans settled');
}
async function enter(path) {
  await tile(path).press('Enter');
  await location().locator('[aria-current="location"]').filter({ hasText: basename(path) }).waitFor();
  await settled();
}
async function unchanged(path, identity) {
  const current = await lstat(path);
  assert.equal(current.dev, identity.dev);
  assert.equal(current.ino, identity.ino);
  assert.deepEqual(await readFile(join(path, 'nested', 'owned.bin')), payload);
}
async function focusOnMove() {
  await until(() => moveButton().evaluate(element => element === document.activeElement), 'dialog restores focus to Move to Trash');
}
async function moveOwnedEntry(path, kind) {
  const identity = await lstat(path);
  const destination = join(home, '.Trash', basename(path));
  assert.equal(await absent(destination), true, 'unique owned Trash destination must not preexist');
  assert.equal((await invoke('get_path_details', { path })).canTrash, true);
  const preview = await invoke('preview_trash_entry', { path });
  assert.equal(preview.kind, kind);
  additionalTrash.push({ destination, identity });
  const result = await invoke('trash_entry', { planId: preview.planId, confirmed: true });
  assert.equal(result.movedPath, path);
  await until(() => absent(path), `owned ${kind} moved through native Trash`);
  const moved = await lstat(destination);
  assert.equal(moved.dev, identity.dev);
  assert.equal(moved.ino, identity.ino);
  await assert.rejects(invoke('trash_entry', { planId: preview.planId, confirmed: true }), /expired|过期/i);
  nativeCases.push({ path, destination, kind, dev: moved.dev, ino: moved.ino });
  return destination;
}
async function appearance(theme) {
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('group', { name: 'Appearance', exact: true }).getByRole('button', { name: theme === 'dark' ? 'Dark' : 'Light', exact: true }).click();
  await page.getByRole('button', { name: 'Space explorer', exact: true }).click();
  await until(() => page.evaluate(theme => document.documentElement.dataset.theme === theme, theme), `${theme} theme applied`);
}
async function resize(width, height) {
  await app.evaluate(({ BrowserWindow }, bounds) => BrowserWindow.getAllWindows()[0].setBounds(bounds), { width, height });
  await until(async () => {
    const actual = await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].getBounds());
    return actual.width === width && actual.height === height;
  }, `${width}×${height} native bounds`);
  await delay(250);
}

try {
  await Promise.all([mkdir(join(target, 'nested'), { recursive: true }), mkdir(retained, { recursive: true }), mkdir(missing, { recursive: true }), mkdir(replacement), mkdir(join(evidence, 'user-data')), mkdir(join(evidence, 'codex'))]);
  await Promise.all([writeFile(join(target, 'nested', 'owned.bin'), payload), writeFile(join(retained, 'keep.bin'), randomBytes(1024 * 1024)), writeFile(join(missing, 'owned.bin'), randomBytes(1024 * 1024)), writeFile(join(replacement, 'original.txt'), 'original')]);
  movedIdentity = await lstat(target);
  assert.equal(await absent(trashed), true, 'unique Trash destination must not preexist');
  await writeFile(bootstrap, `
    const { app } = require('electron');
    app.setPath('appData', ${JSON.stringify(evidence)});
    app.setPath('userData', ${JSON.stringify(join(evidence, 'user-data'))});
    require(${JSON.stringify(join(project, 'electron/main.cjs'))});
  `);
  step = 'launching unmodified Electron main with isolated state';
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
  const originalApplications = await cached(applications);
  await enter(fixture);
  const originalFixture = await invoke('get_disk_scan_view', { scanId: originalApplications.scanId, path: fixture });
  await enter(parent);
  await tile(target).click();
  await until(() => moveButton().isEnabled(), 'selected ordinary folder is eligible for Trash');

  step = 'checking live selected dates against native metadata';
  const details = await invoke('get_path_details', { path: target });
  assert.equal(details.canTrash, true);
  assert.ok(Math.abs(details.createdMs - movedIdentity.birthtimeMs) < 1);
  assert.ok(Math.abs(details.modifiedMs - movedIdentity.mtimeMs) < 1);
  const dates = await page.evaluate(values => values.map(timestamp => new Date(timestamp).toLocaleString('en-US', { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })), [details.createdMs, details.modifiedMs]);
  assert.deepEqual(await panel().locator('.selection-date dd').allTextContents(), dates);
  await page.screenshot({ path: join(evidence, 'selection-live-dates.png') });
  for (const theme of ['light', 'dark']) {
    await appearance(theme);
    for (const [width, height] of [[1180, 820], [900, 620]]) {
      await resize(width, height);
      await panel().locator('h3').scrollIntoViewIfNeeded();
      await page.screenshot({ path: join(evidence, `selection-${theme}-${width}x${height}-dates.png`) });
      await moveButton().scrollIntoViewIfNeeded();
      assert.equal(await moveButton().isVisible(), true);
      const bounds = await moveButton().boundingBox();
      const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight }));
      assert.ok(bounds.x >= 0 && bounds.y >= 0 && bounds.x + bounds.width <= viewport.width && bounds.y + bounds.height <= viewport.height, 'Trash action is reachable inside native viewport');
      await page.screenshot({ path: join(evidence, `selection-${theme}-${width}x${height}-actions.png`) });
    }
  }
  await appearance('light');
  await resize(1180, 820);
  console.log('PASS selection dates match live native birth/modified metadata');

  step = 'opening, cancelling and escaping confirmation without mutation';
  await moveButton().click();
  await dialog().waitFor();
  assert.ok((await dialog().innerText()).includes('This folder and all its contents will be moved to Trash.'));
  assert.ok((await dialog().innerText()).includes('Moving it to Trash does not free disk space; empty Trash afterwards to release it.'));
  await unchanged(target, movedIdentity);
  assert.equal(await absent(trashed), true);
  await page.screenshot({ path: join(evidence, 'trash-confirmation.png') });
  await dialog().getByRole('button', { name: 'Cancel', exact: true }).click();
  await dialog().waitFor({ state: 'hidden' });
  await unchanged(target, movedIdentity);
  await focusOnMove();
  await moveButton().click();
  await dialog().waitFor();
  await page.keyboard.press('Escape');
  await dialog().waitFor({ state: 'hidden' });
  await panel().waitFor();
  await unchanged(target, movedIdentity);
  await focusOnMove();
  console.log('PASS preview, Cancel and Escape preserve directory/content and restore trigger focus');

  step = 'confirming native shell Trash and refreshing measurements';
  await moveButton().click();
  const confirmedAt = Date.now();
  await dialog().getByRole('button', { name: 'Confirm Move to Trash', exact: true }).click();
  await until(() => absent(target), 'original selected directory moved away');
  await unchanged(trashed, movedIdentity);
  assert.equal(await absent(join(retained, 'keep.bin')), false);
  await page.locator('.mac-notice').filter({ hasText: 'Moved to Trash. Empty Trash afterwards to free disk space; restore through Finder before emptying it.' }).waitFor();
  const refreshedParent = await cached(parent, undefined, confirmedAt);
  await settled();
  await until(async () => await tile(target).count() === 0 && await panel().count() === 0, 'post-Trash refreshed UI removes tile and selection');
  await page.screenshot({ path: join(evidence, 'trash-complete-refreshed.png') });
  await location().getByRole('button', { name: basename(fixture), exact: true }).click();
  const refreshedFixture = await cached(fixture);
  await settled();
  assert.equal(originalFixture.scannedFiles - refreshedFixture.scannedFiles, 1);
  assert.equal(originalFixture.allocatedBytes - refreshedFixture.allocatedBytes, movedIdentity.blocks * 512 + (await lstat(join(trashed, 'nested'))).blocks * 512 + (await lstat(join(trashed, 'nested', 'owned.bin'))).blocks * 512);
  await enter(parent);
  assert.equal(await tile(target).count(), 0);
  assert.equal((await cached(parent)).scanId, refreshedParent.scanId, 'fresh parent is reused on ancestor return');
  console.log('PASS real Trash preserves device/inode/content; tile disappears and ancestor totals refresh');

  step = 'showing missing metadata and recovering through Retry';
  await rm(missing, { recursive: true });
  await tile(missing).click();
  await panel().getByText('Item missing or dates could not be read.', { exact: true }).waitFor();
  assert.deepEqual(await panel().locator('.selection-date dd').allTextContents(), ['Unavailable', 'Unavailable']);
  assert.equal(await moveButton().isEnabled(), true, 'missing dates do not gate a live Trash preview');
  await moveButton().click();
  await panel().getByRole('alert').waitFor();
  assert.equal(await dialog().count(), 0, 'missing live item cannot produce a confirmation plan');
  await mkdir(missing);
  await panel().getByRole('button', { name: 'Retry', exact: true }).click();
  await until(async () => !(await panel().locator('.selection-date dd').allTextContents()).some(value => ['Unavailable', 'Loading…'].includes(value)), 'Retry reads restored metadata');
  assert.equal(await moveButton().isEnabled(), true);
  console.log('PASS missing dates keep Trash available; missing live preview errors and Retry recovers dates');

  step = 'recovering an invalidated preview with Check again';
  await moveButton().click();
  await dialog().waitFor();
  const missingOriginal = join(parent, 'Missing-original');
  const missingIdentity = await lstat(missing);
  await rename(missing, missingOriginal);
  await mkdir(missing);
  await writeFile(join(missing, 'replacement.txt'), 'replacement');
  await dialog().getByRole('button', { name: 'Confirm Move to Trash', exact: true }).click();
  await dialog().getByRole('alert').waitFor();
  assert.equal((await lstat(missingOriginal)).ino, missingIdentity.ino);
  assert.equal(await readFile(join(missing, 'replacement.txt'), 'utf8'), 'replacement');
  await dialog().getByRole('button', { name: 'Check again', exact: true }).click();
  await dialog().getByRole('button', { name: 'Confirm Move to Trash', exact: true }).waitFor();
  assert.equal(await readFile(join(missing, 'replacement.txt'), 'utf8'), 'replacement');
  await page.screenshot({ path: join(evidence, 'replacement-rechecked.png') });
  await dialog().getByRole('button', { name: 'Cancel', exact: true }).click();
  await dialog().waitFor({ state: 'hidden' });
  await focusOnMove();
  console.log('PASS replacement rejects move; Check again creates a new preview requiring separate confirmation');

  step = 'rejecting a replaced directory through real native IPC';
  const preview = await invoke('preview_trash_entry', { path: replacement });
  await rename(replacement, savedOriginal);
  await mkdir(replacement);
  await writeFile(join(replacement, 'replacement.txt'), 'replacement');
  await assert.rejects(invoke('trash_entry', { planId: preview.planId, confirmed: true }), /changed|变化/i);
  assert.equal(await readFile(join(replacement, 'replacement.txt'), 'utf8'), 'replacement');
  assert.equal(await readFile(join(savedOriginal, 'original.txt'), 'utf8'), 'original');
  console.log('PASS stale preview cannot Trash a replacement directory');

  step = 'moving owned Library, hidden, app, link and outside-Home entries through real native Trash';
  libraryFixture = await mkdtemp(join(home, 'Library', 'Caches', 'msm-selection-trash-'));
  const suffix = basename(libraryFixture);
  const libraryDirectory = join(libraryFixture, `Library-item-${suffix}`);
  await mkdir(libraryDirectory);
  await writeFile(join(libraryDirectory, 'owned.txt'), 'owned Library cache fixture');
  const movedLibrary = await moveOwnedEntry(libraryDirectory, 'directory');
  assert.equal(await readFile(join(movedLibrary, 'owned.txt'), 'utf8'), 'owned Library cache fixture');

  const hiddenFile = join(libraryFixture, `.hidden-${suffix}`);
  await writeFile(hiddenFile, 'owned hidden fixture');
  const movedHidden = await moveOwnedEntry(hiddenFile, 'file');
  assert.equal(await readFile(movedHidden, 'utf8'), 'owned hidden fixture');

  const appBundle = join(libraryFixture, `Owned-${suffix}.app`);
  await mkdir(join(appBundle, 'Contents'), { recursive: true });
  await writeFile(join(appBundle, 'Contents', 'owned.txt'), 'owned app-shaped fixture');
  const movedApp = await moveOwnedEntry(appBundle, 'directory');
  assert.equal(await readFile(join(movedApp, 'Contents', 'owned.txt'), 'utf8'), 'owned app-shaped fixture');

  const linkTarget = join(libraryFixture, 'kept-link-target.txt');
  await writeFile(linkTarget, 'retained link target');
  const targetIdentity = await lstat(linkTarget);
  const selectedLink = join(libraryFixture, `Link-${suffix}`);
  await symlink(linkTarget, selectedLink);
  const movedLink = await moveOwnedEntry(selectedLink, 'symlink');
  assert.equal((await lstat(movedLink)).isSymbolicLink(), true);
  assert.equal(await readlink(movedLink), linkTarget);
  assert.equal((await lstat(linkTarget)).ino, targetIdentity.ino);
  assert.equal(await readFile(linkTarget, 'utf8'), 'retained link target');

  const hardlink = join(libraryFixture, `Hardlink-${suffix}`);
  await link(linkTarget, hardlink);
  const movedHardlink = await moveOwnedEntry(hardlink, 'file');
  assert.equal((await lstat(movedHardlink)).ino, targetIdentity.ino);
  assert.equal(await readFile(linkTarget, 'utf8'), 'retained link target');

  const outsideHome = join(evidence, `Outside-Home-${suffix}`);
  await writeFile(outsideHome, 'owned temporary fixture outside Home');
  const movedOutsideHome = await moveOwnedEntry(outsideHome, 'file');
  assert.equal(await readFile(movedOutsideHome, 'utf8'), 'owned temporary fixture outside Home');
  await assert.rejects(invoke('preview_trash_file', { path: linkTarget }), /Unsupported|malformed/i);
  await assert.rejects(invoke('trash_file', { file: { path: linkTarget } }), /Unsupported|malformed/i);
  assert.equal(await readFile(linkTarget, 'utf8'), 'retained link target');
  await writeFile(join(evidence, 'native-trash-cases.json'), JSON.stringify({ cases: nativeCases, retainedLinkTarget: { path: linkTarget, dev: targetIdentity.dev, ino: targetIdentity.ino }, legacyApisRejected: true }, null, 2));
  assert.deepEqual(errors, []);
  console.log('PASS native Trash accepts Library cache, hidden file, app bundle and outside-Home file; symlink target and hardlink peer remain intact');
  console.log(`Native evidence: ${evidence}`);
} catch (error) {
  if (page) await page.screenshot({ path: join(evidence, 'failure.png') }).catch(() => {});
  console.error(`FAIL ${step}; native evidence: ${evidence}`);
  throw error;
} finally {
  if (app) await app.close();
  // Never clean a Trash entry unless it is still exactly the fixture we moved.
  if (movedIdentity && !(await absent(trashed))) {
    const stat = await lstat(trashed);
    if (stat.dev === movedIdentity.dev && stat.ino === movedIdentity.ino) await rm(trashed, { recursive: true });
  }
  for (const { destination, identity } of additionalTrash) {
    if (!(await absent(destination))) {
      const current = await lstat(destination);
      if (current.dev === identity.dev && current.ino === identity.ino) await rm(destination, { recursive: true });
    }
  }
  if (libraryFixture) await rm(libraryFixture, { recursive: true, force: true });
  await rm(fixture, { recursive: true, force: true });
}
