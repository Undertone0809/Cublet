// Real development Electron + Rust, owned files, isolated profile/cache.
// Build renderer and debug core first. Evidence stays outside the app repository.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { access, lstat, mkdir, mkdtemp, open, readFile, readdir, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir, userInfo } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';
import { setTimeout as delay } from 'node:timers/promises';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin', 'Native macOS required');
const project = fileURLToPath(new URL('..', import.meta.url));
const outputRoot = resolve(process.env.MSM_DETAIL_EVIDENCE_ROOT ?? tmpdir());
await mkdir(outputRoot, { recursive: true });
const evidence = await mkdtemp(join(outputRoot, 'native-detail-'));
const applications = await realpath(join(userInfo().homedir, 'Applications'));
const fixture = await mkdtemp(join(applications, 'Cublet detail fixture-'));
const workspaces = join(fixture, 'Workspaces');
const buildCache = join(workspaces, 'Build cache');
const compiler = join(buildCache, 'Compiler output');
const packages = join(buildCache, 'Package store');
const references = join(fixture, 'References');
const referenceMetadata = join(references, 'Metadata tables');
const smallFile = join(buildCache, 'ordinary-note.txt');
const userData = join(evidence, 'user-data');
const cache = join(evidence, 'cache');
const core = resolve(process.env.CODEX_CORE_PATH ?? join(project, 'rust-core/target/debug/codex-core'));
const bootstrap = join(evidence, 'main.cjs');
const env = { ...process.env, CODEX_CORE_PATH: core, CODEX_HOME: join(evidence, 'codex'), MSM_DISK_CACHE_ROOT: cache };
for (const key of ['ELECTRON_RUN_AS_NODE', 'ELECTRON_START_URL', 'ELECTRON_REMOTE_DEBUGGING_PORT']) delete env[key];
const result = { status: 'running', evidence, fixture, mode: 'development native Electron/Rust with owned synthetic filesystem fixture', assertions: [], screenshots: [], errors: [] };
const productVersion = JSON.parse(await readFile(join(project, 'package.json'), 'utf8')).version;
let app, page, identity, windowId, step = 'preparing the fixture';
const MiB = 1024 * 1024;
const invoke = (method, params = {}) => page.evaluate(({ method, params }) => window.electronAPI.invoke(method, params), { method, params });
const tile = path => page.locator(`[data-directory-tile=${JSON.stringify(path)}]`).getByRole('button').first();
const navigation = () => page.getByRole('navigation', { name: 'Current path', exact: true });
const status = () => page.locator('.analysis-status');
const selection = () => page.getByRole('complementary', { name: 'Selected item', exact: true });
const check = description => result.assertions.push({ description, at: new Date().toISOString() });
async function until(read, description, timeout = 120_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) { const value = await read(); if (value) return value; await delay(40); }
  throw Error(`Timed out: ${description}`);
}
async function allocate(path, bytes) {
  await mkdir(join(path, '..'), { recursive: true });
  const handle = await open(path, 'w');
  try {
    for (let left = bytes; left > 0; left -= MiB) await handle.writeFile(randomBytes(Math.min(left, MiB)));
    await handle.sync();
  } finally { await handle.close(); }
}
async function allocation(path) {
  const stat = await lstat(path, { bigint: true });
  let bytes = Number(stat.blocks) * 512;
  if (stat.isDirectory()) for (const name of await readdir(path)) bytes += await allocation(join(path, name));
  return bytes;
}
async function launch() {
  console.log('Launching isolated native fixture');
  app = await _electron.launch({ args: [bootstrap], cwd: project, env });
  page = await app.firstWindow(); page.setDefaultTimeout(20_000);
  identity = await app.evaluate(({ app, BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0];
    window.setBounds({ x: 100, y: 80, width: 1280, height: 820 }); window.show(); window.focus();
    return { version: app.getVersion(), appPath: app.getAppPath(), packaged: app.isPackaged, userData: app.getPath('userData'), pid: process.pid };
  });
  identity.electronVersion = identity.version;
  identity.productVersion = productVersion;
  delete identity.version;
  assert.equal(await realpath(identity.userData), await realpath(userData));
  page.on('pageerror', error => result.errors.push(String(error)));
  await page.waitForFunction(() => !!window.electronAPI);
  console.log('Native bridge ready');
  windowId = undefined;
  try {
    windowId = execFileSync('/usr/bin/swift', ['-e', `import CoreGraphics
let rows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
let pid = Int(CommandLine.arguments[1])!
if let row = rows.first(where: { ($0[kCGWindowOwnerPID as String] as? Int) == pid && ($0[kCGWindowLayer as String] as? Int) == 0 }) { print(row[kCGWindowNumber as String]!) }
`, String(identity.pid)], { encoding: 'utf8' }).trim();
    assert.match(windowId, /^\d+$/);
  } catch (error) { result.nativeCaptureLimitation = String(error); windowId = undefined; }
  result.runtime = identity;
  console.log(`Native capture ready: ${windowId ?? 'renderer fallback'}`);
}
async function completed(previousId) {
  return until(async () => {
    const scan = await invoke('get_cached_disk_scan', { path: fixture, allowProtected: true });
    if (scan?.status !== 'complete' || scan.scanId === previousId) return null;
    if (await page.getByRole('button', { name: 'Stop scan', exact: true }).count()) return null;
    return scan;
  }, 'completed exact fixture scan');
}
async function persistedSnapshot() {
  const process = spawn(core, [], { env, stdio: ['pipe', 'pipe', 'pipe'] });
  const lines = createInterface({ input: process.stdout });
  try {
    const response = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(Error('Persisted snapshot RPC timed out')), 20_000);
      lines.on('line', line => {
        const value = JSON.parse(line);
        if (value.id !== 'cache-proof') return;
        clearTimeout(timer);
        if (value.ok) resolve(value.result); else reject(Error(value.error));
      });
      process.once('error', reject);
    });
    process.stdin.write(`${JSON.stringify({ id: 'cache-proof', method: 'get_cached_disk_scan', params: { path: fixture, allowProtected: true } })}\n`);
    return await response;
  } finally { lines.close(); process.kill(); }
}
async function at(path) {
  await until(async () => (await navigation().getAttribute('title')) === path, `visible scope ${basename(path)}`);
  await page.getByRole('button', { name: 'Rescan', exact: true }).waitFor();
  await until(() => page.getByRole('button', { name: 'Rescan', exact: true }).isEnabled(), 'navigation settled');
}
async function visibleLabel(path) {
  await tile(path).waitFor();
  const label = tile(path).locator('.treemap-name-text');
  await label.waitFor({ state: 'visible' });
  assert.equal(await label.textContent(), basename(path));
  const box = await label.boundingBox();
  assert(box && box.width >= 35 && box.height >= 8, `Named item needs a usable visible label: ${path}`);
}
async function capture(name, action, expected) {
  await page.mouse.move(18, 18);
  if (name !== 'failure') await page.evaluate(async () => {
    const finite = document.getAnimations().filter(animation => animation.effect?.getComputedTiming().iterations !== Infinity);
    await Promise.race([Promise.all(finite.map(animation => animation.finished.catch(() => {}))), new Promise(resolve => setTimeout(resolve, 2000))]);
  });
  const order = result.screenshots.length + 1;
  const path = join(evidence, `${String(order).padStart(2, '0')}-${name}.png`);
  let surface = 'native macOS window';
  if (windowId) {
    try { execFileSync('/usr/sbin/screencapture', ['-x', '-o', '-l', windowId, path]); }
    catch (error) { result.nativeCaptureLimitation = String(error); surface = 'Electron renderer with real native IPC'; await page.screenshot({ path }); }
  } else { surface = 'Electron renderer with real native IPC'; await page.screenshot({ path }); }
  result.screenshots.push({ order, path, action, expected, surface, mode: result.mode, identity, assertionsThrough: result.assertions.length });
}
async function overview() {
  await at(fixture);
  await visibleLabel(compiler); await visibleLabel(packages);
  assert.equal(await navigation().getAttribute('title'), fixture, 'Third-level labels must be visible without drilling');
}
async function enter(path, action) {
  if (action === 'Enter') await tile(path).press('Enter'); else await tile(path).dblclick();
  await at(path);
  check(`${action} enters the represented real directory ${basename(path)}`);
}

try {
  await Promise.all([access(core), access(join(project, 'dist/index.html')), mkdir(userData, { recursive: true })]);
  await writeFile(bootstrap, `const { app } = require('electron'); app.setPath('appData', ${JSON.stringify(evidence)}); app.setPath('userData', ${JSON.stringify(userData)}); require(${JSON.stringify(join(project, 'electron/main.cjs'))});`);
  const assets = (await readdir(join(project, 'dist/assets'))).map(name => join(project, 'dist/assets', name));
  result.artifacts = await Promise.all([core, join(project, 'dist/index.html'), ...assets].map(async path => ({ path, sha256: createHash('sha256').update(await readFile(path)).digest('hex') })));
  await Promise.all([
    allocate(join(compiler, 'compiled-data.bin'), 16 * MiB), allocate(join(packages, 'package-data.bin'), 8 * MiB),
    allocate(join(workspaces, 'Documents', 'documents.bin'), 8 * MiB), allocate(join(fixture, 'Media', 'media.bin'), 12 * MiB), allocate(smallFile, 4096),
    allocate(join(referenceMetadata, 'Index storage', 'reference-index.bin'), 3 * MiB),
  ]);
  for (let start = 0; start < 120; start += 20) await Promise.all(Array.from({ length: 20 }, (_, offset) => allocate(join(fixture, 'Small archive', `stable-larger-${start + offset}.bin`), 64 * 1024)));
  result.oracle = { totalBytes: await allocation(fixture), compilerBytes: await allocation(compiler), packagesBytes: await allocation(packages), ordinaryBytes: await allocation(smallFile) };
  step = 'seeding the isolated saved scope';
  await launch();
  assert.equal((await invoke('get_disk_permissions')).status, 'granted', 'Native permission must permit fixture scanning');
  await page.evaluate(path => {
    localStorage.setItem('mac-storage-manager.scan-scope.v1', path);
    localStorage.setItem('mac-storage-manager.permission-choice.v1', 'full');
    localStorage.setItem('codex-storage-manager.locale.v1', 'en');
    localStorage.setItem('codex-storage-manager.theme.v1', 'dark');
  }, fixture);
  await app.close(); app = undefined;
  await launch();
  step = 'showing named third-level items directly in Current usage';
  const original = await completed();
  assert.equal(original.allocatedBytes, result.oracle.totalBytes, 'Allocated bytes must match independent lstat blocks');
  assert(Array.isArray(original.treemapEntries), 'The candidate must publish the additive treemap directory projection');
  for (const [path, expected] of [[compiler, result.oracle.compilerBytes], [packages, result.oracle.packagesBytes]]) {
    const entry = original.treemapEntries.find(row => row.path === path);
    assert(entry, `The overview projection must retain measured level-3 directory ${basename(path)}`);
    assert.equal(entry.allocatedBytes, expected);
  }
  check('The completed overview includes named level-3 directories; totals match independent allocated blocks');
  await overview();
  await capture('overview-depth-three', 'Open Current usage at the fixture scope', 'Compiler output and Package store are visibly named at level 3 before any drill');
  await tile(compiler).click(); await selection().waitFor();
  assert.equal(await selection().locator('code').textContent(), compiler);
  await selection().getByRole('button', { name: 'Open folder', exact: true }).waitFor();
  await capture('third-level-selection', 'Single-click Compiler output in the original overview', 'Shared selection shows the exact level-3 directory and its folder actions');
  await selection().getByRole('button', { name: 'Close details', exact: true }).click();
  await selection().waitFor({ state: 'hidden' });
  await enter(compiler, 'Enter');
  await page.getByRole('button', { name: /^compiled-data\.bin,/ }).waitFor();
  await capture('third-level-enter', 'Press Enter on Compiler output', 'The real compiler directory opens and compiled-data.bin is visible');
  await page.keyboard.press('Alt+ArrowLeft'); await at(buildCache);
  await visibleLabel(compiler); await visibleLabel(packages);
  await capture('back-parent', 'Use Back after directly entering a third-level folder', 'The immediate Build cache parent returns with its two named children');
  await navigation().getByRole('button', { name: basename(fixture), exact: true }).click(); await overview();
  await capture('breadcrumb-overview', 'Choose the fixture ancestor breadcrumb', 'The original scope returns with its named third-level details');
  await enter(buildCache, 'double-click');
  await visibleLabel(compiler); await visibleLabel(packages);
  await capture('intermediate-double-click', 'Double-click the Build cache parent header', 'The intermediate scope opens with both named child directories');
  await page.getByRole('button', { name: 'Directory details', exact: true }).click();
  const directoryDialog = page.getByRole('dialog', { name: 'Build cache', exact: true });
  await directoryDialog.getByText('ordinary-note.txt', { exact: true }).waitFor();
  await capture('ordinary-file-details', 'Open Directory details at the intermediate scope', 'The live direct listing includes ordinary-note.txt outside the retained largest-file sample');
  await directoryDialog.getByRole('button', { name: 'Close', exact: true }).click();
  await directoryDialog.waitFor({ state: 'hidden' });
  await navigation().getByRole('button', { name: basename(fixture), exact: true }).click(); await overview();
  check('Back and direct ancestor breadcrumb restore the original overview');
  step = 'checking wide light and minimum dark native appearance';
  await page.evaluate(() => { localStorage.setItem('codex-storage-manager.theme.v1', 'light'); document.documentElement.classList.remove('dark'); document.documentElement.dataset.theme = 'light'; document.documentElement.style.colorScheme = 'light'; });
  await invoke('set_window_appearance', { theme: 'light' });
  await overview();
  result.wideReferenceExpanded = await tile(references).evaluate(button => button.classList.contains('is-header'));
  await capture('wide-light', 'Show the regular native window in light appearance', 'Large branches show named level-3 details and smaller references retain their applicable detail');
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setBounds({ width: 900, height: 620 }));
  await page.evaluate(() => { localStorage.setItem('codex-storage-manager.theme.v1', 'dark'); document.documentElement.classList.add('dark'); document.documentElement.dataset.theme = 'dark'; document.documentElement.style.colorScheme = 'dark'; });
  await invoke('set_window_appearance', { theme: 'dark' });
  await overview();
  result.compactReferenceExpanded = await tile(references).evaluate(button => button.classList.contains('is-header'));
  assert.equal(result.wideReferenceExpanded, true, 'The medium reference branch must expand at regular size');
  assert.equal(result.compactReferenceExpanded, false, 'The medium reference branch must collapse below the geometry threshold');
  await tile(references).click(); await selection().waitFor();
  assert.equal(await selection().locator('code').textContent(), references);
  await selection().getByRole('button', { name: 'Close details', exact: true }).click();
  check('A medium branch collapses at minimum size while its real-directory selection remains actionable');
  await capture('minimum-window-dark', 'Resize the native window to 900 by 620 in dark appearance', 'Large third-level directories remain readable; smaller branches collapse when available geometry requires');
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setBounds({ width: 1280, height: 820 }));
  step = 'restarting with the completed hierarchy in cache';
  await app.close(); app = undefined;
  const persisted = await persistedSnapshot();
  assert.equal(persisted.allocatedBytes, original.allocatedBytes);
  for (const path of [compiler, packages]) assert(persisted.treemapEntries?.some(entry => entry.path === path), 'The projection must be rehydrated from persisted cache before a new scan');
  check('A fresh sidecar rehydrates projected level-3 directories from persisted cache before UI startup can rescan');
  await launch();
  const restored = await completed(); await overview();
  assert.equal(restored.allocatedBytes, original.allocatedBytes);
  check('A full Electron and sidecar restart restores the same measured hierarchy');
  result.restart = { originalScan: original.scanId, restoredScan: restored.scanId, cacheHit: restored.cacheHit };
  await capture('restart-cached-detail', 'Restart the app with the isolated persisted cache', 'The same named level-3 detail returns with identical allocation');
  step = 'interrupting a real refresh and recovering';
  for (let start = 0; start < 20000; start += 100) await Promise.all(Array.from({ length: 100 }, (_, offset) => mkdir(join(fixture, 'Small archive', `empty-${start + offset}`))));
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  const stop = page.getByRole('button', { name: 'Stop scan', exact: true });
  await stop.waitFor(); await stop.click();
  await until(async () => (await status().innerText()).includes('Update stopped'), 'cancelled refresh preserves previous result');
  await overview();
  check('Stopping a real native refresh keeps the useful completed hierarchy');
  await capture('interrupted-refresh', 'Stop a genuine scan of the expanded owned fixture', 'Previous results remain visible with Update stopped and Retry scan');
  await page.getByRole('button', { name: 'Retry scan', exact: true }).click();
  const recovered = await completed(restored.scanId); await overview();
  assert.equal(recovered.allocatedBytes, await allocation(fixture));
  result.recovered = { scanId: recovered.scanId, allocatedBytes: recovered.allocatedBytes, coverageComplete: recovered.coverageComplete, issueCount: recovered.issueCount, issues: recovered.issues };
  await capture('recovered-refresh', 'Retry the interrupted scan', 'The completed scan restores level-3 detail and correct independently checked bytes');
  check('Retry completes through native IPC with exact independent allocated-byte totals');
  assert.deepEqual(result.errors, []);
  result.status = 'passed';
  console.log(`PASS native treemap level-3 detail, selection, Enter/double-click, Back, minimum window, restart and interrupted-refresh recovery. Evidence: ${evidence}`);
} catch (error) {
  result.status = 'failed'; result.failedStep = step; result.error = String(error.stack ?? error);
  console.error(`FAIL ${step}: ${result.error}. Evidence: ${evidence}`);
  await writeFile(join(evidence, 'manifest.json'), `${JSON.stringify(result, null, 2)}\n`);
  if (page) await Promise.race([capture('failure', step, String(error)).catch(() => {}), delay(5000)]);
  throw error;
} finally {
  await Promise.race([app?.close().catch(() => {}), delay(5000)]);
  await rm(fixture, { recursive: true, force: true });
  result.finishedAt = new Date().toISOString();
  await writeFile(join(evidence, 'manifest.json'), `${JSON.stringify(result, null, 2)}\n`);
}
