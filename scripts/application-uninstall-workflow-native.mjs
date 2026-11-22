// Build the renderer and debug sidecar first. All mutations target fixture files.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { copyFile, lstat, mkdir, mkdtemp, readFile, realpath, rename, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir, userInfo } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin');
const project = fileURLToPath(new URL('..', import.meta.url));
const evidenceParent = await realpath(process.env.CUBLET_EVIDENCE_ROOT || tmpdir());
const evidence = await mkdtemp(join(evidenceParent, 'application-workflow-'));
const workspace = await mkdtemp(join(await realpath('/tmp'), 'cublet-fx-'));
const bootstrap = join(workspace, 'bootstrap.cjs');
const home = join(workspace, 'home');
const applications = join(home, 'Applications');
const cacheDirectory = join(home, 'Library', 'Caches');
const core = join(project, 'rust-core', 'target', 'debug', 'codex-core');
const env = {
  ...process.env, HOME: home, CODEX_CORE_PATH: core,
  MSM_DISK_CACHE_ROOT: join(workspace, 'cache'), CODEX_HOME: join(workspace, 'codex'),
};
for (const key of ['ELECTRON_RUN_AS_NODE', 'ELECTRON_START_URL', 'ELECTRON_REMOTE_DEBUGGING_PORT']) delete env[key];
const id = randomUUID().slice(0, 8);
const commonId = `com.example.cubletfixture.${id}-shared`;
const singleId = `com.example.cubletfixture.${id}-single`;
const namedId = `com.example.cubletfixture.${id}-named`;
const trashId = `com.example.cubletfixture.${id}-trash`;
const names = {
  main: 'Fixture Rudder.app',
  backup: 'Fixture Rudder.app.pre-test',
  candidate: 'Fixture Rudder Candidate.app',
  single: 'Fixture Solo.app',
  named: 'Fixture Named.app',
  trash: `Fixture Trash ${id}.app`,
};
const paths = {
  main: join(applications, names.main),
  backup: join(applications, names.backup),
  candidate: join(applications, 'candidate', names.candidate),
  single: join(applications, names.single),
  named: join(applications, names.named),
  trash: join(applications, names.trash),
  shared: join(cacheDirectory, commonId),
  exclusive: join(cacheDirectory, singleId),
  http: join(home, 'Library', 'HTTPStorages', singleId),
  namedData: join(home, 'Library', 'Application Support', 'Fixture Named'),
  uncheckedData: join(home, 'Library', 'Application Support', 'Fixture Display Name'),
  untouched: join(home, 'untouched.txt'),
};
const screenshots = [];
const timings = [];
const terminalErrors = [];
let electron;
let page;
let mcp;
let trashLocation;
let step = 'fixture';

async function bundle(target, bundleId, displayName, helpers = false) {
  const exec = join(target, 'Contents', 'MacOS', 'Fixture');
  await mkdir(dirname(exec), { recursive: true });
  await copyFile('/bin/sleep', exec);
  const info = join(target, 'Contents', 'Info.plist');
  await writeFile(info, `<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleIdentifier</key><string>${bundleId}</string>
<key>CFBundleExecutable</key><string>Fixture</string>
<key>CFBundleDisplayName</key><string>${displayName}</string>
<key>CFBundleShortVersionString</key><string>1.2.3</string>
</dict></plist>`);
  if (helpers) {
    for (const suffix of ['helper', 'helper-gpu']) {
      await bundle(join(target, 'Contents', 'Frameworks', `Fixture ${suffix}.app`), `${bundleId}.${suffix}`, 'Fixture Helper');
    }
    await symlink(paths.untouched, join(target, 'Contents', 'Untouched reference'));
  }
}

async function itemDir(target) {
  await mkdir(target, { recursive: true });
  await writeFile(join(target, 'sample.txt'), 'owned fixture');
}

async function missing(target) {
  await assert.rejects(lstat(target), { code: 'ENOENT' });
}

async function restoreFixtureFromTrash(identity) {
  for (const directory of [join(userInfo().homedir, '.Trash'), join(home, '.Trash')]) {
    const candidate = join(directory, basename(paths.trash));
    try {
      const actual = await lstat(candidate, { bigint: true });
      if (actual.ino === identity.ino && actual.dev === identity.dev) trashLocation = candidate;
    } catch (error) { if (!['ENOENT', 'EACCES', 'EPERM'].includes(error.code)) throw error; }
  }
  assert(trashLocation, 'The exact fixture must be found in the native Trash');
  await rename(trashLocation, paths.trash);
  trashLocation = undefined;
  assert.equal((await lstat(paths.trash, { bigint: true })).ino, identity.ino);
}

function matchExact(text) {
  return { exact: true, name: text };
}

function mcpClient(args) {
  const child = spawn(core, args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
  child.on('error', error => terminalErrors.push(String(error)));
  child.stderr.on('data', data => terminalErrors.push(String(data)));
  const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  let nextId = 0;
  async function request(method, params = {}) {
    const id = ++nextId;
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    const line = await lines.next();
    assert.equal(line.done, false, 'MCP exited before responding');
    const response = JSON.parse(line.value);
    assert.equal(response.id, id);
    assert.equal(response.error, undefined);
    return response.result;
  }
  return {
    child,
    async connect() {
      await request('initialize', { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'fixture', version: '1' } });
      child.stdin.write(JSON.stringify({jsonrpc: '2.0', method: 'notifications/initialized'}) + '\n');
    },
    list: () => request('tools/list'),
    async call(name, args = {}, expectedError = false) {
      const result = await request('tools/call', { name, arguments: args });
      assert.equal(Boolean(result.isError), expectedError, JSON.stringify(result));
      return result.structuredContent ?? (expectedError ? result.content[0].text : JSON.parse(result.content[0].text));
    },
  };
}

try {
  await mkdir(applications, { recursive: true });
  await writeFile(paths.untouched, 'preserve this file');
  for (const key of ['main', 'backup', 'candidate']) await bundle(paths[key], commonId, 'Fixture Rudder', key === 'backup');
  await bundle(paths.single, singleId, 'Fixture Solo', true);
  await bundle(paths.named, namedId, 'Fixture Display Name');
  await bundle(paths.trash, trashId, 'Fixture Trash');
  await itemDir(paths.shared);
  await itemDir(paths.exclusive);
  await itemDir(paths.http);
  await itemDir(paths.namedData);
  await itemDir(paths.uncheckedData);
  const httpNested = join(paths.http, 'nested');
  await itemDir(httpNested);
  const deep = join(paths.single, 'Contents', 'Resources', 'Nested');
  await itemDir(deep);
  await writeFile(bootstrap, `
const { app } = require('electron');
app.setPath('home', ${JSON.stringify(home)});
app.setPath('userData', ${JSON.stringify(join(workspace, 'profile'))});
require(${JSON.stringify(join(project, 'electron', 'main.cjs'))});
`);
  const { version } = JSON.parse(await readFile(join(project, 'package.json'), 'utf8'));
  await writeFile(join(workspace, 'package.json'), JSON.stringify({ name: 'cublet-native-fixture', version, main: basename(bootstrap) }));
  console.log('Evidence directory:', evidence);
  electron = await _electron.launch({ args: [workspace], cwd: project, env, timeout: 25_000 });
  page = await electron.firstWindow();
  page.setDefaultTimeout(30_000);
  page.on('pageerror', error => terminalErrors.push(String(error)));
  await page.addInitScript(() => {
    localStorage.setItem('codex-storage-manager.locale.v1', 'en');
    localStorage.setItem('codex-storage-manager.theme.v1', 'light');
    localStorage.setItem('mac-storage-manager.permission-choice.v1', 'limited');
  });
  await page.reload();
  const navigate = name => page.getByRole('navigation', { name: 'Main navigation' }).getByRole('button', matchExact(name)).click();
  const dialog = page.getByRole('dialog');
  const appTable = page.getByRole('table', { name: 'Applications', exact: true });
  const appRow = target => appTable.getByRole('row').filter({ has: page.getByText(target, { exact: true }) });
  const item = target => dialog.getByRole('listitem').filter({ has: page.getByText(target, { exact: true }) });
  const search = page.getByRole('textbox', matchExact('Search applications by name or path'));
  const invoke = (method, params) => page.evaluate(({ method, params }) => window.electronAPI.invoke(method, params), { method, params });

  let nativeWindow;
  if (process.env.CUBLET_CAPTURE_WINDOW === '1') {
    const helper = join(workspace, 'window.swift');
    await writeFile(helper, `
import CoreGraphics
import Foundation
let pid = Int(CommandLine.arguments[1])!
let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
if let window = windows.first(where: { ($0[kCGWindowOwnerPID as String] as? Int) == pid && ($0[kCGWindowLayer as String] as? Int) == 0 }),
   let id = window[kCGWindowNumber as String] as? Int { print(id) }
`);
    const pid = await electron.evaluate(() => process.pid);
    nativeWindow = execFileSync('/usr/bin/swift', [helper, String(pid)], { encoding: 'utf8' }).trim();
    assert.match(nativeWindow, /^\d+$/, 'The development macOS window must be visible');
  }
  async function capture(filename, observed) {
    const path = join(evidence, filename);
    if (nativeWindow) execFileSync('/usr/sbin/screencapture', ['-x', '-o', '-l', nativeWindow, path]);
    else await page.screenshot({ path, animations: 'disabled', scale: 'css' });
    screenshots.push({ path, observed, mode: nativeWindow ? 'development native window' : 'development native IPC renderer' });
  }
  async function showApplication(query, target) {
    const start = performance.now();
    await search.fill(query);
    await appRow(target).waitFor();
    timings.push({ scenario: query, toExactRowMs: Math.round(performance.now() - start) });
  }
  async function preview(target) {
    await appRow(target).getByRole('checkbox').check();
    await page.getByRole('button', matchExact('Review uninstall')).click();
    await item(target).waitFor();
  }

  step = 'APP-01 search';
  await navigate('Applications');
  await showApplication('Fixture Rudder', paths.backup);
  await appRow(paths.main).waitFor();
  await appRow(paths.candidate).waitFor();
  assert.equal(await page.getByRole('combobox', { name: 'Scan scope' }).count(), 0);
  assert.equal(await appTable.getByRole('row').filter({ hasText: 'Fixture Helper' }).count(), 0);
  await capture('01-search.png', 'Independent search finds three distinct app bundles and hides helpers.');

  step = 'APP-02 shared preview and permanent confirmation';
  await preview(paths.backup);
  assert.equal(await item(paths.shared).getByRole('checkbox').isDisabled(), true);
  const deleteButton = dialog.getByRole('button', matchExact('Delete permanently'));
  assert.equal(await deleteButton.isEnabled(), true, await dialog.innerText());
  await capture('02-shared-preview.png', 'Shared cache is visible and protected. Backup is the only selected target.');
  await deleteButton.click();
  await dialog.getByText('Permanently deleted: 1', { exact: true }).waitFor();
  await missing(paths.backup);
  await lstat(paths.main);
  await lstat(paths.candidate);
  assert.equal(await readFile(join(paths.shared, 'sample.txt'), 'utf8'), 'owned fixture');
  assert.equal(await readFile(paths.untouched, 'utf8'), 'preserve this file');
  await capture('03-permanent-result.png', 'One backup was deleted; other copies and shared cache survived.');
  await dialog.getByRole('button', matchExact('Done')).click();
  await dialog.waitFor({ state: 'hidden' });

  step = 'APP-05 descendant replacement and recovery';
  await showApplication('Fixture Solo', paths.single);
  await preview(paths.single);
  await item(paths.exclusive).getByRole('checkbox').check();
  await item(paths.http).getByRole('checkbox').check();
  await capture('04-exclusive-preview.png', 'Only explicitly selected cache and HTTP storage are added to the uninstall.');
  const added = join(deep, 'added-after-preview.txt');
  await writeFile(added, 'new user content');
  await deleteButton.click();
  await dialog.getByRole('button', matchExact('Review remaining items')).waitFor();
  assert.equal(await readFile(added, 'utf8'), 'new user content');
  await lstat(paths.exclusive);
  await lstat(paths.http);
  await capture('05-changed-recovery.png', 'A new nested file invalidated deletion; remaining items can be reviewed.');
  await dialog.getByRole('button', matchExact('Review remaining items')).click();
  await item(paths.single).waitFor();
  const exclusiveCheck = item(paths.exclusive).getByRole('checkbox');
  const httpCheck = item(paths.http).getByRole('checkbox');
  if (!(await exclusiveCheck.isChecked())) await exclusiveCheck.check();
  if (!(await httpCheck.isChecked())) await httpCheck.check();
  await writeFile(join(httpNested, 'changed.txt'), 'another new file');
  await deleteButton.click();
  await dialog.getByText('Permanently deleted: 2', { exact: true }).waitFor();
  await Promise.all([missing(paths.single), missing(paths.exclusive)]);
  assert.equal(await readFile(join(httpNested, 'changed.txt'), 'utf8'), 'another new file');
  await capture('06-partial-result.png', 'App and cache were deleted. Changed HTTP storage was retained.');
  await dialog.getByRole('button', matchExact('Review remaining items')).click();
  await item(paths.single).getByText('App bundle already removed.', { exact: true }).waitFor();
  await item(paths.http).getByRole('checkbox').check();
  await capture('07-remaining-review.png', 'Fresh preview lets the user select the one related item that remained.');
  await deleteButton.click();
  await dialog.getByText('Permanently deleted: 1', { exact: true }).waitFor();
  await missing(paths.http);
  await capture('08-full-uninstall.png', 'The remaining selected item was deleted on explicit re-confirmation.');
  await dialog.getByRole('button', matchExact('Done')).click();
  await dialog.waitFor({ state: 'hidden' });

  step = 'name-matched Application Support selection and permanent deletion';
  await showApplication('Fixture Named', paths.named);
  await preview(paths.named);
  const namedData = item(paths.namedData);
  const nameCheck = namedData.getByRole('checkbox');
  await namedData.getByText('Matched by app name only', { exact: true }).waitFor();
  assert.equal(await nameCheck.isEnabled(), true);
  assert.equal(await nameCheck.isChecked(), false);
  await capture('08a-name-match-preview.png', 'Name-matched folders are visible, selectable and unchecked by default.');
  await nameCheck.check();
  await dialog.getByRole('button', matchExact('Check again')).click();
  await item(paths.named).waitFor();
  assert.equal(await nameCheck.isChecked(), true);
  assert.equal(await item(paths.uncheckedData).getByRole('checkbox').isChecked(), false);
  await capture('08b-name-match-selected.png', 'The selected folder remains checked after rechecking the preview.');
  await deleteButton.click();
  await dialog.getByText('Permanently deleted: 2', { exact: true }).waitFor();
  await Promise.all([missing(paths.named), missing(paths.namedData)]);
  assert.equal(await readFile(join(paths.uncheckedData, 'sample.txt'), 'utf8'), 'owned fixture');
  await capture('08c-name-match-result.png', 'The app and checked folder were deleted. The unchecked folder was retained.');
  await dialog.getByRole('button', matchExact('Done')).click();
  await dialog.waitFor({ state: 'hidden' });

  step = 'APP-03 and APP-06 Trash preference across desktop and MCP';
  await navigate('Settings');
  const mode = page.getByRole('combobox', matchExact('Cleanup action'));
  await mode.click();
  await page.getByRole('option', matchExact('Move to Trash')).click();
  await page.waitForFunction(async () => (await window.electronAPI.invoke('get_cleanup_disposition', {})).mode === 'trash');
  await capture('09-trash-settings.png', 'Settings persisted Move to Trash.');
  await navigate('Applications');
  await showApplication('Fixture Trash', paths.trash);
  await preview(paths.trash);
  const trashButton = dialog.getByRole('button', matchExact('Move to Trash'));
  await trashButton.waitFor();
  await capture('10-trash-preview.png', 'The same confirmation now announces Move to Trash.');
  const identity = await lstat(paths.trash, { bigint: true });
  await trashButton.click();
  await dialog.getByText('Moved to Trash: 1', { exact: true }).waitFor();
  await missing(paths.trash);
  await capture('11-trash-result.png', 'Desktop reports one move to Trash, and the original path is absent.');
  await restoreFixtureFromTrash(identity);
  await dialog.getByRole('button', matchExact('Done')).click();
  await dialog.waitFor({ state: 'hidden' });

  step = 'APP-06 and APP-08 advanced MCP authorization and same strategy';
  mcp = mcpClient(['--mcp', '--allow-cleanup']);
  await mcp.connect();
  const tools = (await mcp.list()).tools.map(tool => tool.name);
  assert(tools.includes('execute_app_uninstall'));
  assert.equal((await mcp.call('get_cleanup_disposition')).mode, 'trash');
  const results = await mcp.call('search_applications', { query: 'Fixture Trash' });
  assert(results.applications.some(app => app.path === paths.trash));
  const plan = await mcp.call('preview_app_uninstall', { paths: [paths.trash] });
  assert.equal(plan.mode, 'trash');
  const appItem = plan.items.find(entry => entry.path === paths.trash);
  assert.equal(appItem.blockedReason, undefined);
  const wrong = { planId: plan.planId, selectedIds: [appItem.id], mode: 'trash', confirmed: false };
  await mcp.call('execute_app_uninstall', wrong, true);
  await lstat(paths.trash);
  const done = await mcp.call('execute_app_uninstall', { ...wrong, confirmed: true });
  assert.deepEqual(done.movedAppPaths, [paths.trash]);
  await missing(paths.trash);
  await restoreFixtureFromTrash(identity);
  const replay = await mcp.call('execute_app_uninstall', { ...wrong, confirmed: true });
  assert.deepEqual(replay, done, 'A retry must read the receipt and retain the restored fixture');
  assert.equal((await lstat(paths.trash, { bigint: true })).ino, identity.ino);
  const desktopPolicy = await invoke('get_cleanup_disposition', {});
  assert.equal(desktopPolicy.mode, 'trash');
  const compact = mcpClient(['--mcp', '--profile', 'cleanup', '--allow-cleanup']);
  try {
    await compact.connect();
    const blocked = await compact.call('reclaim_space', { scope: applications, targetAvailableBytes: 9_000_000_000_000_000 }, true);
    assert.match(String(blocked), /Move to Trash.*No files were changed/);
    assert.equal((await lstat(paths.trash, { bigint: true })).ino, identity.ino);
  } finally {
    compact.child.stdin.end();
  }

  step = '900 × 620 dark mode';
  await navigate('Settings');
  await page.getByRole('button', matchExact('Dark')).click();
  await electron.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(900, 620));
  await navigate('Applications');
  await showApplication('Fixture Rudder', paths.main);
  assert.equal(await appTable.evaluate(table => table.scrollWidth <= table.clientWidth + 1), true);
  await capture('12-dark-window.png', 'Independent search and exact paths remain usable at the minimum dark window.');
  assert.deepEqual(terminalErrors, []);
  console.log('PASS real Electron IPC, native permanent and Trash, shared data, changed descendant recovery, same-policy MCP authorization');
} catch (error) {
  if (page) {
    await page.screenshot({ path: join(evidence, 'failure.png'), timeout: 4000 }).catch(() => {});
    const visible = await page.getByRole('dialog').innerText({ timeout: 500 }).catch(() => '');
    if (visible) console.error(visible.slice(0, 2000));
  }
  throw new Error(`Application workflow failed at ${step}`, { cause: error });
} finally {
  if (mcp) mcp.child.stdin.end();
  await electron?.close();
  // Unlink an exact, uniquely named owned fixture only, never enumerate user Trash.
  if (trashLocation) await rename(trashLocation, paths.trash);
  await rm(workspace, { recursive: true, force: true });
  const sha256 = async file => createHash('sha256').update(await readFile(file)).digest('hex');
  await writeFile(join(evidence, 'manifest.json'), JSON.stringify({
    source: 'Cublet development app; owned synthetic bundles and data',
    appCommit: execFileSync('/usr/bin/git', ['rev-parse', 'HEAD'], { cwd: project, encoding: 'utf8' }).trim(),
    indexTree: execFileSync('/usr/bin/git', ['write-tree'], { cwd: project, encoding: 'utf8' }).trim(),
    coreSha256: await sha256(core),
    rendererSha256: await sha256(join(project, 'dist', 'index.html')),
    timings, screenshots, lastStep: step,
  }, null, 2));
}
