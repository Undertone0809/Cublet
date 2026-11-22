// Run after npm run build and a debug codex-core build on macOS.
// Uses real Electron IPC, Rust traversal and a private cache; changes only its own fixture.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { constants } from 'node:fs';
import { access, lstat, mkdir, mkdtemp, open, readFile, readdir, realpath, rename, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir, userInfo } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin', 'This test requires the native macOS metadata reader');
const project = fileURLToPath(new URL('..', import.meta.url));
const core = resolve(process.env.CODEX_CORE_PATH ?? join(project, 'rust-core/target/debug/codex-core'));
const electron = createRequire(import.meta.url)('electron');
await Promise.all([
  access(core, constants.X_OK), access(join(project, 'dist/index.html')),
  access(electron, constants.X_OK),
  access(join(dirname(electron), '..', 'Frameworks', 'Electron Framework.framework', 'Electron Framework')),
]);
const home = await realpath(userInfo().homedir);
const applications = join(home, 'Applications');
assert((await lstat(applications)).isDirectory(), 'Create ~/Applications before running this test');
const root = await mkdtemp(join(await realpath(tmpdir()), 'msm-comparison-smoke-'));
const evidenceRoot = resolve(process.env.MSM_COMPARISON_EVIDENCE_ROOT ?? root);
await mkdir(evidenceRoot, { recursive: true });
const screenshots = [];
const fixture = await mkdtemp(join(applications, 'msm-comparison-smoke-'));
const growing = join(fixture, 'Growing');
const shrinking = join(fixture, 'Shrinking');
const growthFile = join(growing, 'growth.bin');
const reductionFile = join(shrinking, 'reduction.bin');
const bootstrap = join(root, 'main.cjs');
const userData = join(root, 'user-data');
const cacheRoot = join(root, 'cache');
const env = { ...process.env, CODEX_CORE_PATH: core, CODEX_HOME: join(root, 'codex'), MSM_DISK_CACHE_ROOT: cacheRoot };
delete env.ELECTRON_START_URL;
delete env.ELECTRON_REMOTE_DEBUGGING_PORT;
delete env.ELECTRON_RUN_AS_NODE;
const MiB = 1024 * 1024;
const historyOnly = process.env.MSM_COMPARISON_HISTORY_ONLY === '1';
const independentMeasurements = [];
const errors = [];
let app;
let page;
let step = 'creating allocated native fixtures';
let blockedPrevious;
let backedUpPrevious;
let previousMoved = false;
let outcome = "failed";

async function allocate(path, mebibytes, append = false) {
  const file = await open(path, append ? 'a' : 'w');
  try {
    // Random data avoids sparse holes and compression-friendly synthetic totals.
    for (let i = 0; i < mebibytes; i += 1) await file.writeFile(randomBytes(MiB));
    await file.sync();
  } finally { await file.close(); }
}
const allocated = async (path) => Number((await lstat(path, { bigint: true })).blocks) * 512;
const formatBytes = bytes => {
  const index = bytes ? Math.min(Math.floor(Math.log(bytes) / Math.log(1000)), 4) : 0;
  return `${(bytes / 1000 ** index).toFixed(index ? 1 : 0)} ${['B', 'KB', 'MB', 'GB', 'TB'][index]}`;
};
const invoke = (method, params = {}) => page.evaluate(({ method, params }) => window.electronAPI.invoke(method, params), { method, params });
async function comparison(scan, path = fixture, baselineId) {
  return invoke('get_disk_scan_comparison', { scanId: scan.scanId, path, ...(baselineId ? { baselineId } : {}) });
}
const historyFor = scan => invoke('list_disk_scan_history', { scanId: scan.scanId });
const savedLabel = snapshot => new Date(snapshot.updatedAtMs).toLocaleString('en-US', {
  year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit', fractionalSecondDigits: 3,
});
const baselineControl = () => page.getByRole('combobox', { name: 'Growth baseline', exact: true });
async function chooseBaseline(value, history) {
  const snapshot = value.startsWith('snapshot:')
    ? history.snapshots.find(entry => entry.id === value.slice('snapshot:'.length)) : undefined;
  const label = snapshot ? savedLabel(snapshot) : value === 'day' ? 'Start of today' : 'Previous scan';
  assert(!value.startsWith('snapshot:') || snapshot, 'The requested baseline must be listed as a retained snapshot');
  await baselineControl().click();
  const option = page.getByRole('option', { name: label, exact: true });
  await option.waitFor();
  assert.equal(await option.count(), 1, 'Saved scan times must identify this fixture baseline unambiguously');
  await option.click();
  await page.getByRole('listbox').waitFor({ state: 'hidden' });
  assert.equal((await baselineControl().innerText()).trim(), label);
  assert(await baselineControl().evaluate(element => element === document.activeElement), 'Choosing a baseline must return focus to the Mova control');
}
async function chooseScope(label) {
  await page.getByRole('combobox', { name: 'Scan scope', exact: true }).click();
  await page.getByRole('option', { name: label, exact: true }).click();
}
async function growingAllocated() {
  const children = await readdir(growing);
  return (await allocated(growing)) + (await Promise.all(children.map(name => allocated(join(growing, name))))).reduce((sum, bytes) => sum + bytes, 0);
}
async function waitForGrowth(bytes) {
  const total = page.getByRole('region', { name: 'Changes in this scan' }).locator('.comparison-total b');
  await page.waitForFunction(({ expected }) => document.querySelector('.scan-comparison .comparison-total b')?.textContent === expected,
    { expected: `+${formatBytes(bytes)}` });
  assert.equal(await total.textContent(), `+${formatBytes(bytes)}`);
}

async function waitForCached(previousId) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const current = (await invoke('get_cached_disk_scan', { path: applications, allowProtected: false }))
      ?? (await invoke('get_cached_disk_scan', { path: applications, allowProtected: true }));
    if (current?.status === 'complete' && current.scanId !== previousId) return current;
    await delay(100);
  }
  throw new Error('The selected source scan did not complete within two minutes');
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
  // The real limited-access startup uses /Applications. Select only ~/Applications;
  // never choose the default Home scope during this smoke test.
  const scope = page.getByRole('combobox', { name: 'Scan scope' });
  await scope.click();
  await page.getByRole('option', { name: 'User Applications', exact: true }).click();
}
async function clickGrowth() {
  await page.getByRole('group', { name: 'Analysis mode' }).getByRole('button', { name: 'Growth', exact: true }).click();
  await page.getByRole('region', { name: 'Changes in this scan' }).waitFor();
}
async function drillToGrowing() {
  const comparison = page.getByRole('region', { name: 'Changes in this scan' });
  const location = comparison.getByRole('navigation', { name: 'Comparison path' }).locator('[aria-current="location"]');
  const currentName = (await location.innerText()).trim();
  if (currentName === 'Growing') return;
  const tile = path => page.locator(`[data-directory-tile=${JSON.stringify(path)}]`).getByRole('button');
  if (currentName !== basename(fixture)) await tile(fixture).press('Enter');
  await tile(growing).press('Enter');
  await location.filter({ hasText: /^Growing$/ }).waitFor();
}
async function screenshot(name) {
  const path = join(evidenceRoot, `${String(screenshots.length + 1).padStart(2, '0')}-${name}.png`);
  await page.screenshot({ path, fullPage: true });
  screenshots.push({ path, name, action: step, evidenceMode: 'development Electron with native IPC and isolated allocated fixture',
    viewport: await page.evaluate(() => ({ width: innerWidth, height: innerHeight, theme: document.documentElement.dataset.theme })) });
}
async function assertSavedEndpoint(snapshot) {
  await page.waitForFunction(expected => document.querySelector('[data-testid="growth-saved-endpoint"] time')?.getAttribute('datetime') === expected,
    new Date(snapshot.updatedAtMs).toISOString());
  assert.match(await page.getByTestId('growth-saved-endpoint').innerText(), /Latest saved scan/);
}
async function setAppearance(theme) {
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('group', { name: 'Appearance', exact: true }).getByRole('button', { name: theme === 'dark' ? 'Dark' : 'Light', exact: true }).click();
  await page.getByRole('button', { name: 'Space explorer', exact: true }).click();
  await drillToGrowing();
  await page.waitForFunction(theme => document.documentElement.dataset.theme === theme, theme);
}
async function windowSize(width, height) {
  await app.evaluate(({ BrowserWindow }, size) => BrowserWindow.getAllWindows()[0].setBounds(size), { width, height });
  await page.waitForFunction(size => innerWidth === size.width && innerHeight === size.height, { width, height });
}
async function waitForTerminal(scanId) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const scan = await invoke('get_disk_scan', { scanId });
    if (scan.status !== 'running') return scan;
    await delay(80);
  }
  throw new Error('The native scan did not settle within two minutes');
}
async function waitForScanStart(after) {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const starts = await app.evaluate(() => globalThis.__msmScanStarts);
    if (starts.length > after) return starts.at(-1);
    await delay(20);
  }
  throw new Error('The UI did not start a native scan');
}

try {
  await Promise.all([mkdir(growing), mkdir(shrinking), mkdir(userData), mkdir(join(root, 'codex'))]);
  const settingsFolder = join(cacheRoot, 'com.zeeland.codex-storage-manager');
  await mkdir(settingsFolder, { recursive: true });
  await writeFile(join(settingsFolder, 'disk-cache-limit'), '64\n');
  const settingsProbe = spawnSync(core, [], {
    env, timeout: 5000, encoding: 'utf8',
    input: `${JSON.stringify({ id: 'comparison-cache', method: 'get_disk_cache_settings', params: {} })}\n`,
  });
  assert.ifError(settingsProbe.error);
  assert.equal(settingsProbe.status, 0, settingsProbe.stderr);
  const settings = JSON.parse(settingsProbe.stdout);
  assert.equal(settings.ok, true, settings.error);
  assert.deepEqual(settings.result, { limitMiB: 64, usedBytes: 0 }, 'A debug core honoring the isolated cache is required');
  await writeFile(bootstrap, `
    const { app, shell, ipcMain } = require('electron');
    globalThis.__msmComparisonEvidence = [];
    globalThis.__msmScanStarts = [];
    const registerHandle = ipcMain.handle.bind(ipcMain);
    ipcMain.handle = (channel, handler) => registerHandle(channel, async (event, request) => {
      const result = await handler(event, request);
      if (channel === 'core:invoke' && request?.method === 'start_disk_scan') {
        globalThis.__msmScanStarts.push({ params: request.params, scanId: result?.scanId });
      }
      if (channel === 'core:invoke' && request?.method === 'get_disk_scan_comparison' && result?.refreshing) {
        globalThis.__msmComparisonEvidence.push({ baselineId: result.baselineId, currentId: result.currentId });
      }
      return result;
    });
    globalThis.__msmRevealedPaths = [];
    shell.showItemInFolder = path => { globalThis.__msmRevealedPaths.push(path); };
    app.setPath('appData', ${JSON.stringify(root)});
    app.setPath('userData', ${JSON.stringify(userData)});
    require(${JSON.stringify(join(project, 'electron/main.cjs'))});
  `);
  await allocate(growthFile, 1);
  await allocate(reductionFile, historyOnly ? 3 : 81);
  const oldGrowth = await allocated(growthFile);
  const oldReduction = await allocated(reductionFile);
  const baselineGrowingAllocated = await growingAllocated();

  step = 'scanning the first native snapshot';
  await launch();
  const baseline = await waitForCached();
  assert.equal(baseline.roots.length, 1);
  assert.equal(baseline.roots[0], applications);
  assert.equal((await comparison(baseline)).status, 'no-baseline');
  await clickGrowth();
  await page.getByText('Earliest saved record pinned; complete another scan to compare', { exact: true }).waitFor();
  await assertSavedEndpoint((await historyFor(baseline)).current);
  await screenshot('first-scan');
  await page.getByRole('button', { name: 'Current usage', exact: true }).click();

  step = 'measuring independent growth and reduction';
  const appendedMiB = historyOnly ? 4 : 100;
  await allocate(growthFile, appendedMiB, true);
  const file = await open(reductionFile, 'r+');
  try { await file.truncate(MiB); await file.sync(); } finally { await file.close(); }
  const expectedGrowth = await allocated(growthFile) - oldGrowth;
  const expectedReduction = oldReduction - await allocated(reductionFile);
  // APFS can reserve more blocks than the append's logical length. Measure the
  // actual filesystem allocation; do not confuse a 100 MiB write with its bytes.
  assert.equal((await lstat(growthFile)).size, (1 + appendedMiB) * MiB);
  assert.equal((await lstat(reductionFile)).size, MiB);
  assert(expectedGrowth > 0 && expectedReduction > 0);
  const growthText = formatBytes(expectedGrowth);
  const beforeText = formatBytes(oldGrowth);
  const afterText = formatBytes(await allocated(growthFile));
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  let current = await waitForCached(baseline.scanId);
  const result = await comparison(current);
  assert.equal(result.status, 'ready');
  assert.equal(result.baselineStartedAtMs, baseline.startedAtMs);
  assert.equal(result.growthBytes, expectedGrowth, 'Growth must survive sibling reduction cancellation');
  assert.equal(result.reductionBytes, expectedReduction);
  assert.equal(result.netBytes, expectedGrowth - expectedReduction);
  assert.equal(result.entries.reduce((sum, entry) => sum + entry.growthBytes, 0), expectedGrowth);
  assert.equal(result.entries.reduce((sum, entry) => sum + entry.reductionBytes, 0), expectedReduction);
  assert(result.entries.some(entry => entry.path.startsWith(`${growing}/`) || entry.path === growing));
  assert(result.entries.some(entry => entry.path.startsWith(`${shrinking}/`) || entry.path === shrinking));
  await clickGrowth();
  const changes = page.getByRole('region', { name: 'Changes in this scan' });
  await changes.locator('.comparison-total').waitFor();
  const firstHistory = await historyFor(current);
  const firstSaved = firstHistory.snapshots.find(snapshot => snapshot.startedAtMs === baseline.startedAtMs);
  assert(firstSaved, 'The first completed snapshot must remain available as an explicit baseline');
  await chooseBaseline(`snapshot:${firstSaved.id}`, firstHistory);
  await assertSavedEndpoint(firstHistory.current);
  await baselineControl().click();
  await page.getByText('Historical baselines', { exact: true }).waitFor();
  await page.locator('.mova-select-popup[data-open]').evaluate(async element => {
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    await Promise.all(element.getAnimations({ subtree: true }).map(animation => animation.finished.catch(() => {})));
  });
  await screenshot('historical-baseline-menu');
  await page.keyboard.press('Escape');
  await page.getByRole('listbox').waitFor({ state: 'hidden' });
  await waitForGrowth(expectedGrowth);
  assert.equal(await changes.getByRole('button', { name: 'Reduction', exact: true }).count(), 0);
  assert.equal(await changes.getByRole('group', { name: 'Change direction' }).count(), 0);
  assert.equal(await changes.locator('.comparison-selection, .comparison-explanation, .comparison-limit, .comparison-dates').count(), 0,
    'The growth canvas keeps explanations on demand and has no legacy permanent rows');
  await changes.getByRole('button', { name: 'Comparison details', exact: true }).click();
  const comparisonDetails = page.getByRole('dialog', { name: 'Comparison details', exact: true });
  await comparisonDetails.waitFor();
  for (const timestamp of [result.baselineUpdatedAtMs, result.currentUpdatedAtMs]) {
    const date = new Date(timestamp).toLocaleString('en-US', {
      year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit',
    });
    assert((await comparisonDetails.innerText()).includes(date), 'Exact snapshot dates belong in the comparison dialog');
  }
  assert.match(await comparisonDetails.innerText(), /retained|saved/i);
  await comparisonDetails.getByRole('button', { name: 'Close', exact: true }).click();
  await comparisonDetails.waitFor({ state: 'hidden' });
  assert(await changes.getByRole('button', { name: 'Comparison details', exact: true }).evaluate(element => element === document.activeElement), 'Closing the info dialog must restore its trigger focus');
  await screenshot('growth-overview');

  let cumulative = await comparison(current, growing, firstSaved.id);
  if (!historyOnly) {
  step = 'drilling the growth map and inspecting its keyboard tooltip';
  // Stable native treemap directory attributes are shared with Current usage.
  const tile = path => page.locator(`[data-directory-tile=${JSON.stringify(path)}]`).getByRole('button');
  await tile(fixture).press('Enter');
  await changes.locator('.comparison-total').filter({ hasText: `+${growthText}` }).waitFor();
  assert.equal(await tile(shrinking).count(), 0, 'Growth view must exclude reduction-only areas');
  await tile(growing).waitFor();
  assert.equal(await changes.locator('.comparison-partial').count(), Number(result.limited),
    'Limited retained history must have a compact visible indicator');
  const selection = changes.getByRole('complementary', { name: 'Selected item', exact: true });
  await tile(growing).click();
  await selection.waitFor();
  assert.equal(await selection.locator('code').textContent(), growing);
  await selection.getByRole('button', { name: 'Open folder', exact: true }).waitFor();
  await selection.getByRole('button', { name: 'Show in Finder', exact: true }).click();
  assert.equal(await app.evaluate(() => globalThis.__msmRevealedPaths.at(-1)), growing, 'Finder must receive the real selected historical folder path');
  await selection.getByRole('button', { name: 'Close details', exact: true }).click();
  await selection.waitFor({ state: 'hidden' });
  assert(await tile(growing).evaluate(element => element === document.activeElement), 'Closing selection must return focus to its tile');
  await tile(growing).click();
  await selection.waitFor();
  await tile(growing).press('Escape');
  await selection.waitFor({ state: 'hidden' });
  assert(await tile(growing).evaluate(element => element === document.activeElement), 'Escape from a tile must clear selection without losing focus');
  await tile(growing).click();
  await selection.waitFor();
  await selection.getByRole('button', { name: 'Show in Finder', exact: true }).press('Escape');
  await selection.waitFor({ state: 'hidden' });
  assert(await tile(growing).evaluate(element => element === document.activeElement), 'Escape inside selection must return focus to its tile');
  await tile(growing).click();
  await selection.getByRole('button', { name: 'Open folder', exact: true }).click();
  const comparisonPath = changes.getByRole('navigation', { name: 'Comparison path' });
  await comparisonPath.locator('[aria-current="location"]').filter({ hasText: /^Growing$/ }).waitFor();
  await comparisonPath.getByRole('button', { name: basename(fixture), exact: true }).click();
  await comparisonPath.locator('[aria-current="location"]').filter({ hasText: basename(fixture) }).waitFor();
  await tile(growing).dblclick();
  await comparisonPath.locator('[aria-current="location"]').filter({ hasText: /^Growing$/ }).waitFor();
  // The top-100 file sample is deliberately optional: a bounded history may
  // legitimately retain the directory residual instead of this individual file.
  const growthTile = changes.getByRole('button', { name: /^(growth\.bin|Growing|Other growth),/ }).first();
  assert((await growthTile.getAttribute('aria-label')).includes(`+${growthText}`));
  // Navigation can reuse the same tile DOM node. Establish an actual keyboard
  // focus transition after the new map's ResizeObserver/layout publication.
  await growthTile.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await comparisonPath.locator('[aria-current="location"]').focus();
  await page.keyboard.press('Tab'); // Establish keyboard modality (:focus-visible).
  await growthTile.focus();
  const tooltip = changes.getByRole('tooltip');
  await tooltip.waitFor();
  assert((await tooltip.innerText()).includes(growthText));
  assert((await tooltip.innerText()).includes(beforeText));
  assert((await tooltip.innerText()).includes(afterText));
  assert((await tooltip.innerText()).includes(growing), 'The tooltip must expose the real historical folder path');
  await growthTile.click();
  await selection.waitFor();
  assert.equal(await tooltip.count(), 0, 'Selected-tile actions must suppress the duplicate tooltip overlay');
  assert.equal(await selection.evaluate(element => getComputedStyle(element).alignItems), 'stretch', 'Shared selection layout must not inherit a page-specific centered alignment');
  const selectedPath = await selection.locator('code').textContent();
  assert([growing, growthFile].includes(selectedPath), 'Selection must expose a real retained folder/file path, including residual tiles');
  assert(!selectedPath.includes('\0'), 'Historical aggregate IDs must not escape into filesystem actions');
  await selection.getByRole('button', { name: 'Show in Finder', exact: true }).click();
  assert.equal(await app.evaluate(() => globalThis.__msmRevealedPaths.at(-1)), selectedPath, 'Residual and file actions must resolve the visible real path');
  assert.equal(await selection.getByRole('button', { name: 'Browse current contents', exact: true }).count(), Number(selectedPath === growing));
  await selection.getByRole('button', { name: 'Close details', exact: true }).click();
  await selection.waitFor({ state: 'hidden' });
  assert(await growthTile.evaluate(element => element === document.activeElement), 'Residual/file selection close must restore tile focus');
  if (selectedPath === growthFile) {
    const revealedBefore = await app.evaluate(() => globalThis.__msmRevealedPaths.length);
    await growthTile.dblclick();
    assert.equal(await app.evaluate(() => globalThis.__msmRevealedPaths.length), revealedBefore + 1, 'File double-click must reveal it once');
    assert.equal(await app.evaluate(() => globalThis.__msmRevealedPaths.at(-1)), growthFile);
    await selection.getByRole('button', { name: 'Close details', exact: true }).click();
    await selection.waitFor({ state: 'hidden' });
  }
  await growthTile.hover();
  await tooltip.waitFor();
  assert((await tooltip.innerText()).includes(growthText));
  await screenshot('growth-drill');
  await comparisonPath.getByRole('button', { name: basename(fixture), exact: true }).click();
  await comparisonPath.locator('[aria-current="location"]').filter({ hasText: basename(fixture) }).waitFor();
  await changes.getByRole('button', { name: 'Back to previous comparison scope', exact: true }).click();
  await comparisonPath.locator('[aria-current="location"]').filter({ hasText: /^Growing$/ }).waitFor();

  step = 'keeping an explicit first baseline through four further native rescans';
  // Empty directories lengthen real traversal enough to observe an in-flight UI
  // publication. They live only in this fixture and are present before the first
  // additional scan, so their allocation belongs in the measured oracle below.
  const workload = join(fixture, 'Scan workload');
  await mkdir(workload);
  for (let start = 0; start < 4096; start += 128) {
    await Promise.all(Array.from({ length: 128 }, (_, index) => mkdir(join(workload, `empty-${start + index}`))));
  }
  const transient = join(growing, 'temporary-between-scans.bin');
  let previousGrowing = await growingAllocated();
  let sumPositiveSteps = expectedGrowth;
  let lastTinyGrowth = 0;
  let previousEndpoint;
  let latestHistory;
  let observedPendingPair = false;
  let observedNativeRunningPair = false;
  let capturedProgress = false;
  const completedIds = new Set([baseline.scanId, current.scanId]);
  for (let round = 0; round < 4; round += 1) {
    // A transient file is observed by one scan, then removed. Endpoint comparison
    // must not retain its intermediate positive growth after it disappears.
    await allocate(join(growing, `retained-${round}.bin`), 1);
    if (round === 0) await allocate(transient, 7);
    if (round === 1) await rm(transient);
    const measured = await growingAllocated();
    lastTinyGrowth = Math.max(0, measured - previousGrowing);
    sumPositiveSteps += lastTinyGrowth;
    previousGrowing = measured;
    const oldEndpoint = current;
    previousEndpoint = oldEndpoint;
    const oldPair = await comparison(oldEndpoint, growing, firstSaved.id);
    await app.evaluate(() => { globalThis.__msmComparisonEvidence = []; });
    await page.evaluate(() => {
      window.__msmGrowthObserver?.disconnect();
      window.__msmGrowthPending = [];
      window.__msmGrowthObserver = new MutationObserver(() => {
        const comparison = document.querySelector('.scan-comparison');
        if (comparison && [...comparison.querySelectorAll('.comparison-partial')].some(element => element.textContent === 'Updating')) {
          window.__msmGrowthPending.push(comparison.querySelector('.comparison-total b')?.textContent);
        }
      });
      window.__msmGrowthObserver.observe(document.body, { subtree: true, childList: true, characterData: true, attributes: true });
    });
    // Keep saved time labels distinct even on a very fast local filesystem.
    await delay(1050);
    await page.getByRole('button', { name: 'Rescan', exact: true }).click();
    if (!capturedProgress) {
      const updating = changes.locator('.comparison-partial').filter({ hasText: /^Updating$/ });
      try {
        await updating.waitFor({ timeout: 1500 });
        await assertSavedEndpoint((await historyFor(oldEndpoint)).current);
        await screenshot('refresh-retains-saved-comparison');
        capturedProgress = true;
      } catch (error) {
        if (!String(error).includes('Timeout')) throw error;
      }
    }
    current = await waitForCached(oldEndpoint.scanId);
    completedIds.add(current.scanId);
    const pending = await page.evaluate(() => {
      window.__msmGrowthObserver.disconnect();
      return window.__msmGrowthPending;
    });
    observedPendingPair ||= pending.includes(`+${formatBytes(oldPair.growthBytes)}`);
    const runningPairs = await app.evaluate(() => globalThis.__msmComparisonEvidence);
    for (const pair of runningPairs) {
      assert.equal(pair.currentId, oldPair.currentId, 'A running comparison must retain the latest durable complete endpoint');
      assert.equal(pair.baselineId, firstSaved.id, 'A running comparison must retain the chosen historical baseline');
    }
    observedNativeRunningPair ||= runningPairs.length > 0;
    latestHistory = await historyFor(current);
    assert(latestHistory.snapshots.some(snapshot => snapshot.id === firstSaved.id), 'Repeated same-day scans must retain the chosen first baseline');
    assert.equal((await baselineControl().innerText()).trim(), savedLabel(firstSaved), 'Rescan must not reset a selected baseline to Previous scan');
    cumulative = await comparison(current, growing, firstSaved.id);
    assert.equal(cumulative.status, 'ready');
    assert.equal(cumulative.baselineId, firstSaved.id);
    assert.equal(cumulative.currentId, latestHistory.current.id);
    assert.equal(cumulative.baselineStartedAtMs, baseline.startedAtMs);
    assert.equal(cumulative.currentStartedAtMs, current.startedAtMs);
    assert.equal(cumulative.growthBytes, measured - baselineGrowingAllocated, 'Cumulative growth must compare the first endpoint with the latest allocation');
    assert.equal(cumulative.reductionBytes, 0, 'A created-then-deleted intermediate file is absent at both endpoints');
    await drillToGrowing();
    await waitForGrowth(cumulative.growthBytes);
  }
  assert(observedPendingPair, 'At least one real running scan must keep the previous complete endpoint visible');
  assert(observedNativeRunningPair, 'Native IPC must expose at least one real running comparison with its retained complete endpoint IDs');
  assert(cumulative.growthBytes < sumPositiveSteps, 'Cumulative growth must not sum transient positive deltas from intermediate scans');
  assert.equal(completedIds.size, 6, 'The fixture must complete its first baseline and five distinct successor scans');

  step = 'switching between adjacent and cumulative baselines';
  await chooseBaseline('previous', latestHistory);
  const adjacent = await comparison(current, growing);
  assert.equal(adjacent.baselineStartedAtMs, previousEndpoint.startedAtMs);
  assert.equal(adjacent.growthBytes, lastTinyGrowth);
  assert(lastTinyGrowth > 0 && lastTinyGrowth < cumulative.growthBytes);
  await waitForGrowth(lastTinyGrowth);
  await chooseBaseline(`snapshot:${firstSaved.id}`, latestHistory);
  await waitForGrowth(cumulative.growthBytes);
  await chooseBaseline('day', latestHistory);
  await waitForGrowth(cumulative.growthBytes);
  assert.match(await changes.locator('.comparison-range').innerText(), /^Since earliest saved today /, 'Without a pre-midnight snapshot, Today must describe its actual same-day start');
  await chooseBaseline(`snapshot:${firstSaved.id}`, latestHistory);
  await waitForGrowth(cumulative.growthBytes);

  step = 'retaining real paths and focus with a baseline older than the adjacent scan';
  const olderTile = changes.getByRole('button', { name: /^(growth\.bin|Growing|Other growth),/ }).first();
  await olderTile.click();
  await selection.waitFor();
  const olderSelectedPath = await selection.locator('code').textContent();
  assert([growing, growthFile].includes(olderSelectedPath));
  await selection.getByRole('button', { name: 'Show in Finder', exact: true }).click();
  assert.equal(await app.evaluate(() => globalThis.__msmRevealedPaths.at(-1)), olderSelectedPath);
  await selection.getByRole('button', { name: 'Show in Finder', exact: true }).press('Escape');
  await selection.waitFor({ state: 'hidden' });
  assert(await olderTile.evaluate(element => element === document.activeElement), 'Older-baseline selection Escape must restore tile focus');

  step = 'preserving the explicit baseline through metric and source-scope changes';
  await page.getByRole('button', { name: 'Current usage', exact: true }).click();
  await clickGrowth();
  assert.equal((await baselineControl().innerText()).trim(), savedLabel(firstSaved));
  await drillToGrowing();
  await waitForGrowth(cumulative.growthBytes);
  await chooseScope('Applications');
  await page.getByRole('button', { name: 'Stop scan', exact: true }).waitFor({ state: 'hidden' });
  await baselineControl().waitFor();
  await chooseScope('User Applications');
  current = await waitForCached();
  assert.equal((await baselineControl().innerText()).trim(), savedLabel(firstSaved), "A source-scope round trip must restore that scope's selected baseline");
  await drillToGrowing();
  await waitForGrowth(cumulative.growthBytes);

  }

  if (historyOnly) {
    step = 'archiving the chosen baseline before obstructing only the adjacent history path';
    const priorId = current.scanId;
    await page.getByRole('button', { name: 'Rescan', exact: true }).click();
    current = await waitForCached(priorId);
    const history = await historyFor(current);
    assert(history.snapshots.some(snapshot => snapshot.id === firstSaved.id));
    cumulative = await comparison(current, growing, firstSaved.id);
    assert.equal(cumulative.status, 'ready');
    await assertSavedEndpoint(history.current);
    await drillToGrowing();
    await waitForGrowth(cumulative.growthBytes);
  }

  step = 'retaining the saved endpoint after a real history publication failure';
  const beforeFailedAllocation = await growingAllocated();
  const durableHistory = await historyFor(current);
  const durablePair = await comparison(current, growing, firstSaved.id);
  const cacheDirectory = join(settingsFolder, 'disk-scans');
  for (const name of await readdir(cacheDirectory)) {
    if (!name.endsWith('.json') || name.endsWith('.previous.json') || name.endsWith('.partial.json')) continue;
    const candidate = join(cacheDirectory, name);
    const value = JSON.parse(await readFile(candidate, 'utf8'));
    if (value.snapshot?.scanId !== current.scanId) continue;
    blockedPrevious = candidate.slice(0, -5) + '.previous.json';
    break;
  }
  assert(blockedPrevious, 'Locate only this isolated scope cache by its exact scan ID');
  backedUpPrevious = blockedPrevious + '.fixture-backup';
  await rename(blockedPrevious, backedUpPrevious);
  previousMoved = true;
  await mkdir(blockedPrevious);
  let startsBefore = await app.evaluate(() => globalThis.__msmScanStarts.length);
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  const failedStart = await waitForScanStart(startsBefore);
  const failed = await waitForTerminal(failedStart.scanId);
  const afterFailedAllocation = await growingAllocated();
  independentMeasurements.push({ stage: 'history publication failure', before: beforeFailedAllocation, after: afterFailedAllocation });
  assert.equal(afterFailedAllocation, beforeFailedAllocation, 'Independent allocated bytes must stay fixed across the failed publication fixture');
  assert.equal(failed.status, 'complete', 'This is a completed measurement, not an interrupted scan');
  assert.equal(failed.historySaveFailed, true, 'A real archive-rename failure must report an unsaved measurement');
  assert.deepEqual(failed.roots, current.roots);
  assert.deepEqual((await historyFor(failed)).current, durableHistory.current, 'Failed publication preserves the exact durable endpoint');
  assert.deepEqual(await comparison(failed, growing, firstSaved.id), durablePair, 'Failed publication continues the complete saved comparison');
  await page.getByTestId('growth-scan-notice').filter({ hasText: 'Scan completed, but history could not be saved.' }).waitFor();
  assert.equal(await page.getByRole('button', { name: 'Retry scan', exact: true }).count(), 1, 'Growth exposes one recovery action without a duplicate footer retry');
  await assertSavedEndpoint(durableHistory.current);
  await drillToGrowing();
  await waitForGrowth(cumulative.growthBytes);
  await windowSize(900, 620);
  await setAppearance('light');
  await screenshot('history-save-failed-compact-light');
  await setAppearance('dark');
  await screenshot('history-save-failed-compact-dark');
  await windowSize(1180, 780);
  await screenshot('history-save-failed-regular-dark');
  await setAppearance('light');
  await screenshot('history-save-failed-regular-light');

  step = 'retrying the same source scope after restoring history storage';
  await rm(blockedPrevious, { recursive: true });
  await rename(backedUpPrevious, blockedPrevious);
  previousMoved = false;
  blockedPrevious = undefined;
  backedUpPrevious = undefined;
  startsBefore = await app.evaluate(() => globalThis.__msmScanStarts.length);
  await page.getByTestId('growth-retry-scan').click();
  const retryStart = await waitForScanStart(startsBefore);
  assert.equal(retryStart.params.path, applications, 'Retry from a historical drill must scan the source scope rather than the drilled directory');
  assert.equal(retryStart.params.refresh, true);
  assert.equal(retryStart.params.allowProtected, failedStart.params.allowProtected, 'Retry must preserve the exact protection scope');
  const beforeRetryAllocation = await growingAllocated();
  current = await waitForTerminal(retryStart.scanId);
  const afterRetryAllocation = await growingAllocated();
  independentMeasurements.push({ stage: 'same-scope recovery', before: beforeRetryAllocation, after: afterRetryAllocation });
  assert.equal(afterRetryAllocation, beforeRetryAllocation, 'Independent allocated bytes must stay fixed across the retry fixture');
  assert.equal(current.status, 'complete');
  assert.equal(current.historySaveFailed, undefined, 'Successful publication clears the write-failure outcome');
  assert.notEqual(current.scanId, failed.scanId);
  const recoveredHistory = await historyFor(current);
  assert.notEqual(recoveredHistory.current.id, durableHistory.current.id);
  assert.equal(recoveredHistory.current.startedAtMs, current.startedAtMs);
  assert(recoveredHistory.snapshots.some(snapshot => snapshot.id === durableHistory.current.id), 'The previous endpoint becomes a historical baseline after recovery');
  assert(recoveredHistory.snapshots.some(snapshot => snapshot.id === firstSaved.id), 'Recovery retains the chosen older baseline');
  await page.getByTestId('growth-scan-notice').waitFor({ state: 'hidden' });
  await assertSavedEndpoint(recoveredHistory.current);
  await drillToGrowing();
  await waitForGrowth(cumulative.growthBytes);
  await screenshot('history-save-recovered');

  step = 'reopening the selected older baseline after a full app restart';
  const finalExpected = await comparison(current, growing, firstSaved.id);
  await app.close();
  app = undefined;
  await launch();
  const restored = await waitForCached();
  assert.equal(restored.scanId, current.scanId, 'Scope selection must reuse the latest completed scan');
  assert.equal(restored.cacheHit, true);
  const persisted = await comparison(restored, growing, firstSaved.id);
  assert.deepEqual(persisted, finalExpected, 'Explicit comparison partitions, IDs and timestamps must survive sidecar restart');
  await clickGrowth();
  assert.equal((await baselineControl().innerText()).trim(), savedLabel(firstSaved), 'Restart must retain the explicit saved baseline');
  await drillToGrowing();
  await waitForGrowth(cumulative.growthBytes);
  await screenshot('growth-restarted');
  assert.deepEqual(errors, [], 'The real renderer must not produce page errors');
  outcome = 'passed';
  console.log(historyOnly ? 'PASS Focused native saved endpoint, historical baseline menu, real history-save failure, same-scope retry, compact/regular light/dark recovery and persisted comparison after restart' : 'PASS Native saved endpoint, historical baseline menu, real history-save failure and same-scope retry, compact/regular light/dark recovery, growth-only canvas, independent allocated growth/reduction from +100/-80 MiB logical writes, shared selection actions, real Finder paths (shell captured), close/Escape focus, folder double-click, on-demand info, history navigation, six same-day scans, explicit/previous/day baselines, transient-file exclusion, running endpoint stability, scope/metric persistence and persisted comparison after restart');
  console.log(`Native smoke artifacts: ${evidenceRoot}`);
} catch (error) {
  await screenshot('failure').catch(() => {});
  const alerts = await page?.getByRole('alert').allTextContents().catch(() => []);
  if (alerts?.length) console.error(alerts.join('\n'));
  console.error(`Native smoke artifacts: ${evidenceRoot}`);
  throw new Error(`Scan comparison native smoke failed while ${step}`, { cause: error });
} finally {
  await app?.close();
  if (previousMoved && blockedPrevious && backedUpPrevious) {
    await rm(blockedPrevious, { recursive: true, force: true });
    await rename(backedUpPrevious, blockedPrevious);
  }
  await rm(fixture, { recursive: true, force: true });
  await writeFile(join(evidenceRoot, 'manifest.json'), JSON.stringify({
    evidenceMode: 'development Electron native IPC; isolated synthetic filesystem and cache; renderer screenshots do not establish native chrome',
    version: JSON.parse(await readFile(join(project, 'package.json'), 'utf8')).version,
    sourceRevision: spawnSync('git', ['rev-parse', 'HEAD'], { cwd: project, encoding: 'utf8' }).stdout.trim(),
    coreSHA256: createHash('sha256').update(await readFile(core)).digest('hex'),
    outcome, historyOnly, independentMeasurements, lastStep: step, screenshots,
  }, null, 2));
  // Preserve temporary screenshots and isolated cache for diagnosis; no fixture
  // data remains in the user's Applications directory.
}
