// Real macOS IPC and owned filesystem fixture. No bridge or permission mocks.
// Optional MSM_INSTALLED_APP_PATH executes the exact installed bundle.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { randomBytes, createHash } from 'node:crypto';
import { access, mkdir, mkdtemp, open, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir, userInfo } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin');
const project = fileURLToPath(new URL('..', import.meta.url));
const installed = process.env.MSM_INSTALLED_APP_PATH;
const evidence = await mkdtemp(join(resolve(process.env.MSM_STARTUP_EVIDENCE_ROOT ?? tmpdir()), 'msm-startup-'));
const fixture = await mkdtemp(join(await realpath(join(userInfo().homedir, 'Applications')), 'Startup refresh fixture-'));
const cache = join(evidence, 'cache');
const userData = join(evidence, 'user-data');
const bootstrap = join(evidence, 'main.cjs');
const env = { ...process.env };
for (const name of ['ELECTRON_RUN_AS_NODE', 'ELECTRON_START_URL', 'CODEX_CORE_PATH', 'MSM_DISK_CACHE_ROOT']) delete env[name];
const result = { passed: false, mode: installed ? 'installed app, real filesystem fixture' : 'development native IPC, real filesystem fixture', screenshots: [], errors: [], evidence };
let app, page, identity, executablePath;
const invoke = (method, params = {}) => page.evaluate(({ method, params }) => window.electronAPI.invoke(method, params), { method, params });
const tile = name => page.getByRole('button', { name: new RegExp(`^${name},`) }).first();
const status = () => page.locator('.analysis-status');
async function until(read, description) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) { const value = await read(); if (value) return value; await delay(40); }
  throw Error(`Timed out: ${description}`);
}
async function payload(folder, bytes) {
  await mkdir(join(fixture, folder), { recursive: true });
  const file = await open(join(fixture, folder, 'owned.bin'), 'w');
  try { await file.writeFile(randomBytes(bytes)); await file.sync(); } finally { await file.close(); }
}
async function launch() {
  app = await _electron.launch(installed
    ? { executablePath, args: [`--user-data-dir=${userData}`], env }
    : { args: [bootstrap], cwd: project, env: { ...env, MSM_DISK_CACHE_ROOT: cache } });
  identity = await app.evaluate(({ app }) => ({ version: app.getVersion(), packaged: app.isPackaged, path: app.getAppPath(), pid: process.pid }));
  page = await app.firstWindow();
  page.setDefaultTimeout(20_000);
  page.on('pageerror', error => result.errors.push(String(error)));
  await page.waitForFunction(() => !!window.electronAPI);
}
async function completed(previousId) {
  return until(async () => {
    const scan = await invoke('get_cached_disk_scan', { path: fixture, allowProtected: true });
    if (scan?.status !== 'complete' || scan.scanId === previousId) return null;
    if ((await status().innerText()).includes('正在更新') || await page.getByRole('button', { name: '停止扫描', exact: true }).count()) return null;
    return scan;
  }, 'automatic startup measurement completion');
}
async function screenshot(name, action, expected) {
  const path = join(evidence, `${String(result.screenshots.length + 1).padStart(2, '0')}-${name}.png`);
  await page.mouse.move(420, 30);
  await page.screenshot({ path });
  result.screenshots.push({ path, action, expected, mode: result.mode, identity });
}
try {
  await mkdir(userData, { recursive: true });
  if (installed) {
    executablePath = join(installed, 'Contents/MacOS', execFileSync('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleExecutable', join(installed, 'Contents/Info.plist')], { encoding: 'utf8' }).trim());
    result.asarSha256 = createHash('sha256').update(await readFile(join(installed, 'Contents/Resources/app.asar'))).digest('hex');
  } else {
    await access(join(project, 'dist/index.html'));
    await writeFile(bootstrap, `const { app } = require('electron'); app.setPath('userData', ${JSON.stringify(userData)}); require(${JSON.stringify(join(project, 'electron/main.cjs'))});`);
  }
  await payload('Original files', 4 * 1024 * 1024);
  await payload('Unchanged files', 2 * 1024 * 1024);
  // A broad owned tree keeps genuine progress observable across fast machines;
  // do not pause or mock the sidecar to manufacture an updating screenshot.
  for (let start = 0; start < 12000; start += 100) await Promise.all(Array.from({ length: 100 }, (_, offset) => mkdir(join(fixture, 'Unchanged files', `empty-${start + offset}`))));
  // Seed only this isolated profile, then restart so startup runs unmodified.
  await launch();
  assert.equal((await invoke('get_disk_permissions')).status, 'granted', 'Real native permission must permit restoring the saved fixture scope');
  await page.evaluate(path => {
    localStorage.setItem('mac-storage-manager.scan-scope.v1', path);
    localStorage.setItem('mac-storage-manager.permission-choice.v1', 'full');
    localStorage.setItem('codex-storage-manager.locale.v1', 'zh');
    localStorage.setItem('codex-storage-manager.theme.v1', 'dark');
  }, fixture);
  await app.close(); app = undefined;
  await launch();
  const original = await completed();
  assert.equal(original.scannedFiles, 2);
  await tile('Original files').waitFor();
  await screenshot('initial', '首次打开验收目录', '原生扫描显示两个目录、2 个文件');
  await app.close(); app = undefined;
  await payload('Added while closed', 8 * 1024 * 1024);
  await launch();
  await status().getByText(/显示上次结果 · 正在更新/).waitFor();
  await tile('Original files').waitFor();
  await screenshot('reopened-updating', '在应用关闭时新增文件，再次打开', '自动更新，保留上次地图，无需点击重新分析');
  const refreshed = await completed(original.scanId);
  assert.equal(refreshed.cacheHit, true); // read-only cache retrieval marks provenance
  assert.equal(refreshed.scannedFiles, 3);
  assert.equal(refreshed.allocatedBytes - original.allocatedBytes, 8 * 1024 * 1024);
  await tile('Added while closed').waitFor();
  await screenshot('updated', '等待后台检查完成', '新增目录出现在地图，文件数和占用已更新');
  result.original = { scanId: original.scanId, bytes: original.allocatedBytes, files: original.scannedFiles };
  result.refreshed = { scanId: refreshed.scanId, bytes: refreshed.allocatedBytes, files: refreshed.scannedFiles };
  // Reopen a window without restarting Electron, then stop that entry's scan.
  const windowPromise = app.waitForEvent('window');
  await app.evaluate(({ BrowserWindow }) => new Promise(resolve => { const window = BrowserWindow.getAllWindows()[0]; window.once('closed', resolve); window.close(); }));
  await app.evaluate(({ app }) => app.emit('activate'));
  page = await windowPromise;
  page.on('pageerror', error => result.errors.push(String(error)));
  page.setDefaultTimeout(20_000);
  await status().getByText(/显示上次结果 · 正在更新/).waitFor();
  await page.getByRole('button', { name: '停止扫描', exact: true }).click();
  await status().getByText(/显示上次结果 · 更新已停止/).waitFor();
  await tile('Added while closed').waitFor();
  await screenshot('stopped', '重新打开窗口后停止自动更新', '旧地图保留，显示更新已停止和重试扫描');
  await page.getByRole('button', { name: '重试扫描', exact: true }).click();
  const retried = await completed(refreshed.scanId);
  assert.equal(retried.scannedFiles, 3);
  await tile('Added while closed').waitFor();
  await screenshot('retried', '点击重试扫描', '恢复正常结果，新增文件仍可见');
  if (process.env.MSM_CAPTURE_NATIVE_SHELL === '1') {
    await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.show(); window.focus(); });
    const windowId = execFileSync('swift', ['-e', `import CoreGraphics
let rows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
let pid = Int(CommandLine.arguments[1])!
if let row = rows.first(where: { ($0[kCGWindowOwnerPID as String] as? Int) == pid && ($0[kCGWindowLayer as String] as? Int) == 0 }) { print(row[kCGWindowNumber as String]!) }
`, String(identity.pid)], { encoding: 'utf8' }).trim();
    assert.match(windowId, /^\d+$/, 'Native window must be on screen');
    const path = join(evidence, '06-native-shell-recovered.png');
    execFileSync('/usr/sbin/screencapture', ['-x', '-o', '-l', windowId, path]);
    result.screenshots.push({ path, action: 'Native window after retry', expected: 'Native traffic lights, titlebar and completed fixture scan', mode: result.mode, identity });
  }
  assert.deepEqual(result.errors, []);
  result.passed = true;
} catch (error) {
  result.failure = String(error);
  if (page) await screenshot('failure', 'Failed native check', String(error)).catch(() => {});
  throw error;
} finally {
  await writeFile(join(evidence, 'manifest.json'), JSON.stringify(result, null, 2));
  if (app) await app.close();
  await rm(fixture, { recursive: true, force: true });
  console.log(JSON.stringify({ passed: result.passed, evidence, mode: result.mode }));
}
