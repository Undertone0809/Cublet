// Run serially after npm run build and a debug codex-core build on macOS.
// MSM_INSTALLED_APP_PATH=/Applications/...app runs the same flow in that bundle.
// Installed mode uses its real cache without changing the tier or removing data.
// Only this script's unique ~/Applications fixture is changed and removed.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { constants } from 'node:fs';
import { access, lstat, mkdir, mkdtemp, open, readFile, readdir, realpath, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir, userInfo } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin', 'Native macOS metadata and Electron are required');
const project = fileURLToPath(new URL('..', import.meta.url));
const installed = process.env.MSM_INSTALLED_APP_PATH ? await realpath(resolve(process.env.MSM_INSTALLED_APP_PATH)) : undefined;
const evidenceRoot = resolve(process.env.MSM_GROWTH_EVIDENCE_ROOT ?? await realpath(tmpdir()));
await mkdir(evidenceRoot, { recursive: true });
const evidence = await mkdtemp(join(evidenceRoot, 'msm-growth-drill-'));
const applications = join(await realpath(userInfo().homedir), 'Applications');
assert((await lstat(applications)).isDirectory(), 'Create ~/Applications before running this smoke test');
const fixture = await mkdtemp(join(applications, 'msm-growth-drill-'));
const folders = ['New folder', 'Level two', 'Level three', 'Level four'].reduce((paths, name) => [...paths, join(paths.at(-1) ?? fixture, name)], []);
const smallFile = join(folders.at(-1), 'ordinary-small.txt');
const directFile = join(fixture, 'ordinary-direct.txt');
const removedFile = join(fixture, 'delete-large-owned.bin');
const userData = join(evidence, 'user-data');
const bootstrap = join(evidence, 'main.cjs');
const cacheRoot = join(evidence, 'cache');
const env = { ...process.env };
// In particular never pass a debug cache override to the production sidecar.
for (const name of ['ELECTRON_START_URL', 'ELECTRON_REMOTE_DEBUGGING_PORT', 'ELECTRON_RUN_AS_NODE', 'MSM_DISK_CACHE_ROOT', 'CODEX_CORE_PATH']) delete env[name];
const result = { status: 'running', mode: installed ? 'installed' : 'development', evidence, fixture, assertions: [], screenshots: [], pageErrors: [] };
let executablePath;
let app;
let page;
let step = 'preparing owned fixtures';
const MiB = 1024 * 1024;
const allocated = async path => Number((await lstat(path, { bigint: true })).blocks) * 512;
const formatBytes = bytes => {
  const index = bytes ? Math.min(Math.floor(Math.log(bytes) / Math.log(1000)), 4) : 0;
  return `${(bytes / 1000 ** index).toFixed(index ? 1 : 0)} ${['B', 'KB', 'MB', 'GB', 'TB'][index]}`;
};
const invoke = (method, params = {}) => page.evaluate(({ method, params }) => window.electronAPI.invoke(method, params), { method, params });
const changes = () => page.getByRole('region', { name: 'Changes in this scan', exact: true });
const navigation = () => changes().getByRole('navigation', { name: 'Comparison path', exact: true });
const currentLocation = () => navigation().locator('[aria-current="location"]');
const directoryTile = path => changes().locator(`[data-directory-tile=${JSON.stringify(path)}]`).getByRole('button').first();
const fileTile = () => changes().getByRole('button', { name: /^ordinary-small\.txt,/ }).first();
const selection = () => changes().getByRole('complementary', { name: 'Selected item', exact: true });
const baselineControl = () => changes().getByRole('combobox', { name: 'Growth baseline', exact: true });
const savedLabel = snapshot => new Date(snapshot.updatedAtMs).toLocaleString('en-US', {
  year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit', fractionalSecondDigits: 3,
});
const check = description => result.assertions.push({ description, at: new Date().toISOString() });
async function until(read, description) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const value = await read();
    if (value) return value;
    await delay(100);
  }
  throw new Error(`Timed out: ${description}`);
}
async function allocate(path, bytes) {
  const file = await open(path, 'w');
  try {
    for (let remaining = bytes; remaining > 0; remaining -= MiB) await file.writeFile(randomBytes(Math.min(MiB, remaining)));
    await file.sync();
  } finally { await file.close(); }
}
async function treeAllocation(path, rows = new Map()) {
  const stat = await lstat(path, { bigint: true });
  rows.set(path, Number(stat.blocks) * 512);
  if (stat.isDirectory()) for (const name of await readdir(path)) await treeAllocation(join(path, name), rows);
  return rows;
}
async function cached(previousId) {
  return until(async () => {
    const scan = (await invoke('get_cached_disk_scan', { path: applications, allowProtected: false }))
      ?? (await invoke('get_cached_disk_scan', { path: applications, allowProtected: true }));
    return scan?.status === 'complete' && scan.scanId !== previousId ? scan : undefined;
  }, 'completed User Applications scan');
}
async function launch() {
  app = await _electron.launch(installed
    ? { executablePath, args: [`--user-data-dir=${userData}`], cwd: project, env }
    : { args: [bootstrap], cwd: project, env });
  // Capture the exact real path passed to the native shell. Deliberately do not
  // open Finder repeatedly; observing Finder itself is a separate manual check.
  await app.evaluate(({ shell }) => {
    globalThis.__msmGrowthDrillReveals = [];
    shell.showItemInFolder = path => { globalThis.__msmGrowthDrillReveals.push(path); };
  });
  result.runtime = await app.evaluate(({ app }) => ({ version: app.getVersion(), appPath: app.getAppPath(), packaged: app.isPackaged, userData: app.getPath('userData') }));
  assert.equal(await realpath(result.runtime.userData), await realpath(userData), 'The test must not share the normal app profile');
  if (installed) assert.equal(result.runtime.packaged, true, 'Installed verification must execute the packaged app');
  page = await app.firstWindow();
  page.setDefaultTimeout(20_000);
  page.on('pageerror', error => result.pageErrors.push(String(error)));
  await page.addInitScript(() => {
    localStorage.setItem('codex-storage-manager.locale.v1', 'en');
    localStorage.setItem('mac-storage-manager.permission-choice.v1', 'limited');
  });
  await page.reload();
  await page.getByRole('button', { name: 'Rescan', exact: true }).waitFor();
  // The normal limited-access startup may scan /Applications. Never choose Home.
  await page.getByRole('combobox', { name: 'Scan scope', exact: true }).click();
  await page.getByRole('option', { name: 'User Applications', exact: true }).click();
}
async function openGrowth() {
  await page.getByRole('group', { name: 'Analysis mode', exact: true }).getByRole('button', { name: 'Growth', exact: true }).click();
  await changes().waitFor();
}
async function chooseBaseline(snapshot) {
  await baselineControl().click();
  const option = page.getByRole('option', { name: savedLabel(snapshot), exact: true });
  assert.equal(await option.count(), 1, 'The saved baseline label must uniquely identify this measurement');
  await option.click();
  await page.getByRole('listbox').waitFor({ state: 'hidden' });
  await assertBaseline(snapshot);
}
async function assertBaseline(snapshot) {
  assert.equal((await baselineControl().innerText()).trim(), savedLabel(snapshot), 'Drill, Back, breadcrumbs and restart must retain the chosen baseline');
}
const compare = (scan, path, baselineId) => invoke('get_disk_scan_comparison', { scanId: scan.scanId, path, baselineId });
async function atDirectory(path, snapshot) {
  await until(async () => (await currentLocation().getAttribute('title')) === path, `historical scope ${path}`);
  await assertBaseline(snapshot);
  assert.equal(await page.getByRole('button', { name: 'Growth', exact: true }).getAttribute('aria-pressed'), 'true', 'Historical drilling must stay in Growth');
  assert.equal(await changes().getByRole('button', { name: 'Browse current contents', exact: true }).count(), 0, 'New archives must preserve deeper history, not fall back to live contents');
}
async function enter(path, action, snapshot, capture = false) {
  const tile = directoryTile(path);
  await tile.waitFor();
  if (action === 'Enter') await tile.press('Enter');
  else if (action === 'double-click') await tile.dblclick();
  else {
    await tile.click();
    await selection().waitFor();
    assert.equal(await selection().locator('code').textContent(), path);
    if (capture) await screenshot('folder-selection', `Select ${basename(path)}`, 'The real directory path and Open folder action are visible');
    await selection().getByRole('button', { name: 'Open folder', exact: true }).click();
  }
  await atDirectory(path, snapshot);
  check(`${action} opens historical folder ${basename(path)} with the same baseline`);
  if (capture) await screenshot(`enter-${basename(path).replaceAll(' ', '-').toLowerCase()}`, `${action}: ${basename(path)}`, 'The breadcrumb names the entered historical scope and the deeper file remains visible');
}
async function reveal(action) {
  const tile = fileTile();
  await tile.waitFor();
  const countBefore = await app.evaluate(() => globalThis.__msmGrowthDrillReveals.length);
  if (action === 'Enter') await tile.press('Enter');
  else if (action === 'double-click') await tile.dblclick();
  else {
    await tile.click();
    await selection().waitFor();
    assert.equal(await selection().locator('code').textContent(), smallFile, 'Ordinary small-file selection must expose the exact file');
    assert.equal(await selection().getByRole('button', { name: 'Open folder', exact: true }).count(), 0);
    await screenshot('file-selection', 'Select ordinary-small.txt', 'The exact file path and Show in Finder action are visible; the shell call is tested separately');
    await selection().getByRole('button', { name: 'Show in Finder', exact: true }).click();
  }
  await until(async () => (await app.evaluate(() => globalThis.__msmGrowthDrillReveals.length)) > countBefore, `file ${action} shell invocation`);
  const paths = await app.evaluate(() => globalThis.__msmGrowthDrillReveals);
  assert.equal(paths.length, countBefore + 1, 'One activation must invoke Finder exactly once');
  assert.equal(paths.at(-1), smallFile, 'Finder must receive a real small-file path, never a residual ID');
  assert(!paths.at(-1).includes('\0'));
  if (await selection().isVisible()) {
    await selection().getByRole('button', { name: 'Close details', exact: true }).click();
    await selection().waitFor({ state: 'hidden' });
  }
  check(`${action} reveals the actual ordinary small-file path`);
}
async function drill(snapshot) {
  await enter(fixture, 'Enter', snapshot);
  for (let i = 0; i < folders.length; i++) await enter(folders[i], ['Enter', 'double-click', 'Open folder', 'Enter'][i], snapshot);
  await fileTile().waitFor();
}
async function screenshot(name, action = name, expected = 'The asserted state is visible') {
  const order = result.screenshots.length + 1;
  const path = join(evidence, `${String(order).padStart(2, '0')}-${name}.png`);
  if (name !== 'failure') {
    await changes().locator('.comparison-metrics').waitFor();
    await changes().locator('.treemap-hit').first().waitFor();
  }
  // Ignore looping spinners and bound finite-motion settling. Failure evidence
  // must remain capturable even when the page cannot reach its expected state.
  const motion = name === 'failure' ? { skipped: true } : await page.evaluate(async () => {
    const finite = document.getAnimations().filter(animation => animation.effect?.getComputedTiming().iterations !== Infinity);
    let deadline;
    try {
      const settled = await Promise.race([
        Promise.all(finite.map(animation => animation.finished.catch(() => {}))).then(() => true),
        new Promise(resolve => { deadline = setTimeout(() => resolve(false), 2000); }),
      ]);
      return { settled, finiteAnimations: finite.length };
    } finally { clearTimeout(deadline); }
  });
  if (name !== 'failure') assert(motion.settled, 'Finite UI motion did not settle before the screenshot deadline');
  await page.screenshot({ path, fullPage: true });
  result.screenshots.push({ order, action, expected, path, mode: result.mode,
    surface: 'Electron renderer', version: result.runtime?.version,
    artifactHashes: result.artifacts?.map(artifact => artifact.sha256),
    assertionsThrough: result.assertions.length, motion, at: new Date().toISOString() });
}

try {
  await mkdir(userData, { recursive: true });
  if (installed) {
    const probe = spawnSync('/usr/bin/plutil', ['-extract', 'CFBundleExecutable', 'raw', '-o', '-', join(installed, 'Contents/Info.plist')], { encoding: 'utf8' });
    assert.equal(probe.status, 0, probe.stderr);
    executablePath = join(installed, 'Contents/MacOS', probe.stdout.trim());
    await access(executablePath, constants.X_OK);
    const artifactPaths = [join(installed, 'Contents/Resources/app.asar'), join(installed, 'Contents/Resources/bin/codex-core')];
    result.artifacts = await Promise.all(artifactPaths.map(async path => ({ path, sha256: createHash('sha256').update(await readFile(path)).digest('hex') })));
  } else {
    const core = resolve(process.env.CODEX_CORE_PATH ?? join(project, 'rust-core/target/debug/codex-core'));
    await Promise.all([access(core, constants.X_OK), access(join(project, 'dist/index.html')), access(createRequire(import.meta.url)('electron'), constants.X_OK)]);
    env.CODEX_CORE_PATH = core;
    env.CODEX_HOME = join(evidence, 'codex');
    env.MSM_DISK_CACHE_ROOT = cacheRoot;
    await mkdir(join(cacheRoot, 'com.zeeland.codex-storage-manager'), { recursive: true });
    await writeFile(join(cacheRoot, 'com.zeeland.codex-storage-manager/disk-cache-limit'), '64\n');
    await writeFile(bootstrap, `const { app } = require('electron');\napp.setPath('appData', ${JSON.stringify(evidence)});\napp.setPath('userData', ${JSON.stringify(userData)});\nrequire(${JSON.stringify(join(project, 'electron/main.cjs'))});\n`);
  }
  // More than 100 larger allocated files displace both tiny files from the
  // legacy largest-file sample. They remain stable at both measured endpoints.
  for (let start = 0; start < 120; start += 10) await Promise.all(Array.from({ length: 10 }, (_, i) => allocate(join(fixture, `stable-larger-${String(start + i).padStart(3, '0')}.bin`), 64 * 1024)));
  await allocate(removedFile, 16 * MiB);
  const before = await treeAllocation(fixture);
  step = 'retaining a complete first native baseline';
  await launch();
  result.cacheBefore = await invoke('get_disk_cache_settings');
  const baselineScan = await cached();
  assert.deepEqual(baselineScan.roots, [applications]);
  // A production cache may already contain measurements. The owned fixture must
  // be observed in this exact endpoint before using it as the fixed baseline.
  const baselineBytes = [...before.values()].reduce((sum, bytes) => sum + bytes, 0);
  const baselineView = await invoke('get_disk_scan_view', { scanId: baselineScan.scanId, path: fixture }).catch(() => null);
  if (baselineView?.allocatedBytes !== baselineBytes || baselineView?.scannedFiles !== 121) {
    await page.getByRole('button', { name: 'Rescan', exact: true }).click();
    Object.assign(baselineScan, await cached(baselineScan.scanId));
  }
  const measuredBaseline = await invoke('get_disk_scan_view', { scanId: baselineScan.scanId, path: fixture });
  assert.equal(measuredBaseline.allocatedBytes, baselineBytes, 'The fixed baseline must contain the entire owned fixture');
  assert.equal(measuredBaseline.scannedFiles, 121, 'The fixed baseline must include 120 stable files and the large deletion target');
  check('A complete native baseline includes the owned deletion target');

  step = 'adding four new directory levels and ordinary files while deleting the large file';
  await mkdir(folders.at(-1), { recursive: true });
  await Promise.all([allocate(smallFile, 4096), allocate(directFile, 4096)]);
  await rm(removedFile);
  const after = await treeAllocation(fixture);
  const oracle = { growthBytes: 0, reductionBytes: 0, netBytes: 0, smallFileBytes: await allocated(smallFile), directFileBytes: await allocated(directFile) };
  for (const path of new Set([...before.keys(), ...after.keys()])) {
    const delta = (after.get(path) ?? 0) - (before.get(path) ?? 0);
    oracle.growthBytes += Math.max(0, delta);
    oracle.reductionBytes += Math.max(0, -delta);
    oracle.netBytes += delta;
  }
  assert(oracle.smallFileBytes > 0 && oracle.growthBytes > 0 && oracle.netBytes < 0);
  result.oracle = oracle;
  await delay(50); // Ensure two distinct saved measurement timestamps.
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  let current = await cached(baselineScan.scanId);
  assert(!current.entries.some(entry => entry.path === smallFile || entry.path === directFile), 'The tiny files must be outside the source scan largest-file sample');
  const history = await invoke('list_disk_scan_history', { scanId: current.scanId });
  const baseline = history.snapshots.find(snapshot => snapshot.startedAtMs === baselineScan.startedAtMs);
  assert(baseline, 'The exact first baseline must remain retained');
  result.baseline = baseline;
  result.current = history.current;
  const endpoint = await compare(current, fixture, baseline.id);
  assert.equal(endpoint.status, 'ready');
  for (const field of ['growthBytes', 'reductionBytes', 'netBytes']) assert.equal(endpoint[field], oracle[field], `${field} must equal independent per-path allocated-block differences`);
  assert(endpoint.netBytes < 0 && endpoint.growthBytes > 0, 'Growth must remain useful when net usage decreases');
  // These assertions fail the old top-100-only history and shallow absence logic.
  for (const path of [directFile, smallFile]) {
    const entry = endpoint.entries.find(entry => entry.path === path);
    assert(entry, `Historical detail must retain ordinary file ${path}`);
    assert.equal(entry.kind, 'file');
    assert.equal(entry.beforeBytes, 0);
    assert.equal(entry.afterBytes, await allocated(path));
    assert.equal(entry.growthBytes, await allocated(path));
  }
  for (const path of folders) {
    const scoped = await compare(current, path, baseline.id);
    assert.equal(scoped.status, 'ready');
    assert.equal(scoped.baselineId, baseline.id);
    assert(scoped.entries.some(entry => entry.path === smallFile && entry.kind === 'file'), `Absence proof must propagate through ${path}`);
  }
  result.endpoint = endpoint;
  check('Small ordinary files outside top100 survive history; four newly absent ancestors remain drillable despite negative net');

  step = 'verifying native Growth metrics and the full interaction path';
  await openGrowth();
  await chooseBaseline(baseline);
  await enter(fixture, 'Enter', baseline);
  await until(async () => (await changes().locator('.comparison-total b').textContent()) === `+${formatBytes(oracle.growthBytes)}`, 'positive growth metric');
  // Match semantic labels as well as values: minus/net must not replace growth.
  const metricsText = await changes().locator('.comparison-metrics').innerText();
  assert(metricsText.includes('Added usage') && metricsText.includes('Removed usage') && metricsText.includes('Net usage decrease'));
  assert(metricsText.includes(`+${formatBytes(oracle.growthBytes)}`));
  assert(metricsText.includes(`−${formatBytes(oracle.reductionBytes)}`));
  assert(metricsText.includes(formatBytes(Math.abs(oracle.netBytes))));
  await screenshot('negative-net-positive-growth', 'Choose the first baseline and enter the test directory', 'Added and removed usage stay separate; net usage decreases while added files remain in the map');
  for (let i = 0; i < folders.length; i++) await enter(folders[i], ['Enter', 'double-click', 'Open folder', 'Enter'][i], baseline, true);
  await fileTile().waitFor();
  await reveal('Enter');
  await reveal('double-click');
  await reveal('Show in Finder');
  await screenshot('four-level-small-file', 'Activate the deepest ordinary file with Enter, double-click and Show in Finder', 'The fourth-level path and ordinary-small.txt remain visible; shell paths are captured rather than opening Finder');
  await changes().getByRole('button', { name: 'Back to previous comparison scope', exact: true }).click();
  await atDirectory(folders[2], baseline);
  await screenshot('back-to-parent', 'Use Back', 'The previous third-level historical scope is restored with the same baseline');
  await navigation().getByRole('button', { name: basename(fixture), exact: true }).click();
  await atDirectory(fixture, baseline);
  await screenshot('breadcrumb-to-root', 'Click the test-directory breadcrumb', 'The test-directory overview and its added/removed/net metrics return with the same baseline');
  check('Back and breadcrumb restore real historical ancestors without losing baseline');
  // A second traversal exercises controls after Back/breadcrumb state changes.
  for (const [index, path] of folders.entries()) await enter(path, ['Open folder', 'Enter', 'double-click', 'Open folder'][index], baseline);
  await fileTile().waitFor();

  step = 'preserving the same first baseline through a further native rescan';
  await allocate(join(fixture, 'later-small.txt'), 4096);
  await page.getByRole('button', { name: 'Rescan', exact: true }).click();
  current = await cached(current.scanId);
  await assertBaseline(baseline);
  // Rescan must retain navigation to this deep historical scope as well.
  await atDirectory(folders.at(-1), baseline);
  await fileTile().waitFor();
  const finalComparison = await compare(current, folders.at(-1), baseline.id);
  assert.equal(finalComparison.baselineId, baseline.id);
  assert(finalComparison.entries.some(entry => entry.path === smallFile && entry.kind === 'file'));
  check('A third completed endpoint retains the first baseline and deep small-file detail');
  await screenshot('rescan-complete', 'Rescan while browsing the fourth-level directory', 'The completed endpoint preserves the deep historical path, first baseline and ordinary-small.txt');

  step = 'checking identical history and selection after a full app restart';
  await app.close();
  app = undefined;
  await launch();
  const restored = await cached();
  assert.equal(restored.scanId, current.scanId, 'Relaunch should reuse the completed User Applications scan');
  assert.equal(restored.cacheHit, true);
  assert.deepEqual(await compare(restored, folders.at(-1), baseline.id), finalComparison, 'Historical identities, file partitions and values must survive a sidecar restart');
  await openGrowth();
  await assertBaseline(baseline);
  // A restart may reset the view location; start at Scan scope via its crumb.
  if ((await currentLocation().innerText()).trim() !== 'Scan scope') await navigation().getByRole('button', { name: 'Scan scope', exact: true }).click();
  await drill(baseline);
  await reveal('Show in Finder');
  result.cacheAfter = await invoke('get_disk_cache_settings');
  assert.equal(result.cacheAfter.limitMiB, result.cacheBefore.limitMiB, 'Verification must not change the cache tier');
  assert.deepEqual(result.pageErrors, [], 'The real renderer must not produce page errors');
  await screenshot('restarted-small-file', 'Restart the app and traverse the saved comparison again', 'The original baseline and the fourth-level ordinary file survive a native app and sidecar restart');
  check('Restart preserves the fixed baseline, durable historical file detail and native action path');
  result.status = 'passed';
  result.finderEvidence = 'Actual Electron shell invocation paths captured; Finder UI is not opened or visually verified by this script.';
  console.log(`PASS Native Growth deep drill, ordinary files outside top100, positive added usage with negative net, Enter/double-click/Open folder, Back/breadcrumb, exact Finder paths and baseline persistence after rescan/restart. Evidence: ${evidence}`);
} catch (error) {
  result.status = 'failed';
  result.failedStep = step;
  result.error = String(error.stack ?? error);
  await screenshot('failure').catch(() => {});
  result.alerts = await page?.getByRole('alert').allTextContents().catch(() => []);
  console.error(`Growth drill native smoke failed while ${step}. Evidence: ${evidence}`);
  throw error;
} finally {
  await app?.close().catch(() => {});
  await rm(fixture, { recursive: true, force: true });
  result.finishedAt = new Date().toISOString();
  await writeFile(join(evidence, 'result.json'), `${JSON.stringify(result, null, 2)}\n`);
}
