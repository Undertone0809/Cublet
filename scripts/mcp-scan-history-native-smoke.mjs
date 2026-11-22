// Requires macOS, the built renderer/debug sidecar, and 1 GiB of fixture space.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { lstat, mkdir, mkdtemp, open, readFile, readdir, realpath, rename, rm, statfs, writeFile } from 'node:fs/promises';
import { tmpdir, userInfo } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { createInterface } from 'node:readline';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { _electron } from 'playwright';

assert.equal(process.platform, 'darwin');
const project = fileURLToPath(new URL('..', import.meta.url));
const output = await realpath(process.env.CUBLET_EVIDENCE_ROOT || tmpdir());
const evidence = await mkdtemp(join(output, 'mcp-scan-history-'));
const fixture = await mkdtemp(join(await realpath(userInfo().homedir), 'cublet-fixture-'));
const home = fixture;
const applications = join(home, 'Applications');
const repo = join(fixture, 'Sample repo');
const sibling = join(fixture, 'Other repo');
const artifact = join(repo, 'target/debug/incremental/owned.o');
const source = join(repo, 'src/lib.rs');
const cache = join(fixture, 'cache');
const agentCache = join(cache, 'com.zeeland.codex-storage-manager', 'agent-disk-scans');
const bootstrap = join(fixture, 'bootstrap.cjs');
const core = join(project, 'rust-core/target/debug/codex-core');
const env = { ...process.env, HOME: home, CODEX_CORE_PATH: core, MSM_DISK_CACHE_ROOT: cache };
for (const name of ['ELECTRON_START_URL', 'ELECTRON_REMOTE_DEBUGGING_PORT', 'ELECTRON_RUN_AS_NODE']) delete env[name];
const MiB = 1024 * 1024;
const clients = [];
const screenshots = [];
const pageErrors = [];
let app;
let page;
let step = 'fixture';
let result;

const git = (...args) => execFileSync('git', [
  '-C', repo, '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false',
  '-c', 'user.name=Cublet Fixture', '-c', 'user.email=fixture@example.invalid',
  ...args,
], { env, encoding: 'utf8' });
async function available() {
  const { bsize, bavail } = await statfs(repo);
  return bsize * bavail;
}
async function allocation(path) {
  const stat = await lstat(path);
  let bytes = stat.blocks * 512;
  if (stat.isDirectory()) {
    for (const name of await readdir(path)) bytes += await allocation(join(path, name));
  }
  return bytes;
}
async function waitFor(check, label) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    const value = await check();
    if (value) return value;
    await delay(100);
  }
  throw new Error(`Timed out waiting for ${label}`);
}
async function connect(args) {
  const child = spawn(core, ['--mcp', ...args], { env, stdio: ['pipe', 'pipe', 'inherit'] });
  clients.push(child);
  const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  let sequence = 0;
  const send = value => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
  async function request(method, params) {
    const id = ++sequence;
    send({ id, method, params });
    const response = await lines.next();
    assert(!response.done, 'The MCP server exited before responding');
    const frame = JSON.parse(response.value);
    assert.equal(frame.id, id);
    assert.equal(frame.error, undefined);
    return frame.result;
  }
  await request('initialize', { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'history fixture', version: '1' } });
  send({ method: 'notifications/initialized' });
  return async (name, args) => {
    const response = await request('tools/call', { name, arguments: args });
    assert.equal(response.isError, false, response.content[0].text);
    return response.structuredContent ?? JSON.parse(response.content[0].text);
  };
}
async function scan(call, path, allowProtected = false) {
  const { jobId } = await call('scan', { path, allowProtected });
  return waitFor(async () => {
    const response = await call('scan_poll', { jobId, limit: 1 });
    assert(!['failed', 'cancelled'].includes(response.status), JSON.stringify(response));
    return response.status === 'complete' && response.current;
  }, `MCP scan of ${basename(path)}`);
}
const button = name => page.getByRole('button', { name, exact: true });
const dialog = () => page.getByRole('dialog', { name: 'MCP 扫描记录' });
const readHistory = () => page.evaluate(() => window.electronAPI.invoke('get_agent_scan_history', {}));
async function launch() {
  app = await _electron.launch({ args: [bootstrap], cwd: project, env });
  page = await app.firstWindow();
  page.on('pageerror', error => pageErrors.push(String(error)));
  await page.addInitScript(() => {
    localStorage.setItem('codex-storage-manager.locale.v1', 'zh');
    localStorage.setItem('mac-storage-manager.permission-choice.v1', 'limited');
  });
  await page.reload();
  await page.getByRole('combobox', { name: '扫描范围', exact: true }).click();
  await page.getByRole('option', { name: '用户应用程序', exact: true }).click();
  await page.getByRole('group', { name: '分析模式' }).getByRole('button', { name: '新增占用', exact: true }).click();
  await button('MCP 扫描记录').waitFor();
  await page.getByTestId('growth-saved-endpoint').locator('time').waitFor();
  await page.waitForFunction(() => {
    const indicator = document.querySelector('[aria-label="分析模式"] [aria-pressed="true"] .mova-segmented-indicator');
    if (!indicator) return false;
    const pill = indicator.getBoundingClientRect();
    const selected = indicator.parentElement.getBoundingClientRect();
    return Math.abs(pill.left - selected.left) < 1 && Math.abs(pill.right - selected.right) < 1;
  });
}
async function screenshot(name) {
  const path = join(evidence, `${name}.png`);
  if (name !== 'failed' && await dialog().isVisible()) {
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    await page.waitForFunction(() => !document.getAnimations().some(animation =>
      (animation.pending || animation.playState === 'running')
      && animation.effect?.getComputedTiming().iterations !== Infinity));
  }
  await page.screenshot({ path, animations: 'disabled' });
  screenshots.push(path);
}
async function fingerprint() {
  const entries = await readdir(agentCache);
  return Promise.all(entries.sort().map(async name => {
    const stat = await lstat(join(agentCache, name));
    return [name, stat.ino, stat.size, stat.mtimeMs];
  }));
}

try {
  await mkdir(applications, { recursive: true });
  await mkdir(sibling);
  await mkdir(dirname(artifact), { recursive: true });
  await mkdir(dirname(source));
  await writeFile(join(repo, 'Cargo.toml'), '[package]\nname="mcp-history-fixture"\nversion="0.0.0"\nedition="2021"\n');
  await writeFile(source, 'pub fn preserve() {}\n');
  await writeFile(join(repo, '.gitignore'), '/target/\n');
  git('init', '--quiet', '--template=');
  git('add', '.gitignore', 'Cargo.toml', 'src/lib.rs');
  git('commit', '--quiet', '-m', 'Synthetic source control');
  const gitHead = git('rev-parse', 'HEAD');
  const sourceBytes = await readFile(source);
  assert((await available()) >= 1024 * MiB);
  const file = await open(artifact, 'w');
  try {
    for (let i = 0; i < 640; i++) await file.writeFile(randomBytes(MiB));
    await file.sync();
  } finally {
    await file.close();
  }
  await writeFile(join(sibling, 'other.txt'), 'separate scan scope');
  await writeFile(bootstrap, `
    const { app } = require('electron');
    app.setPath('home', ${JSON.stringify(home)});
    app.setPath('appData', ${JSON.stringify(fixture)});
    app.setPath('userData', ${JSON.stringify(join(fixture, 'user-data'))});
    require(${JSON.stringify(join(project, 'electron/main.cjs'))});
  `);
  const cleanup = await connect(['--profile', 'cleanup', '--allow-cleanup']);
  const advanced = await connect([]);

  step = 'entry and a cleanup with zero filesystem changes';
  const noop = await cleanup('reclaim_space', { scope: repo, targetAvailableBytes: 1 });
  assert.equal(noop.executionEvidence.state, 'no_op');
  await launch();
  await screenshot('01-growth-entry');
  await button('MCP 扫描记录').click();
  await dialog().getByText('还没有已完成并保留的 MCP 扫描。', { exact: false }).waitFor();
  assert.deepEqual((await readHistory()).scopes, []);
  await screenshot('02-no-saved-scan');

  step = 'first retained repository scan';
  await scan(advanced, sibling);
  await scan(advanced, repo, true);
  const initial = await scan(advanced, repo);
  const initialAllocation = await allocation(repo);
  assert.equal(initial.allocatedBytes, initialAllocation);
  await dialog().getByRole('button', { name: '刷新记录' }).click();
  await dialog().getByText('同一范围还没有可比的上一次扫描。').waitFor();
  const single = await readHistory();
  const reference = single.scopes.find(scope => scope.id === single.selectedScopeId);
  assert.deepEqual(reference.roots, [repo]);
  assert.equal(reference.allowProtected, false);
  assert.equal(single.comparison, null, 'A different protection policy is not a baseline');
  await screenshot('03-first-scan');

  step = 'real compact-profile cleanup and post-cleanup audit';
  const artifactBytes = await allocation(artifact);
  const targetAvailableBytes = (await available()) + 32 * MiB;
  let outcome = await cleanup('reclaim_space', { scope: repo, targetAvailableBytes });
  const operationId = outcome.operationId;
  await waitFor(async () => {
    if (['completed', 'partial', 'failed', 'cancelled', 'interrupted'].includes(outcome.state)) return true;
    outcome = await cleanup('cleanup_status', { operationId, waitMs: 500 });
    return false;
  }, 'cleanup completion');
  let cleanupEvidence = 'compact profile cleanup and automatic audit';
  if (outcome.diagnostics?.samples?.some(sample => sample.code === 'candidate_inspection_unavailable')) {
    assert.equal(outcome.deletedFiles, 0);
    assert.equal(await allocation(artifact), artifactBytes);
    assert.deepEqual(await readHistory(), single, 'A refused cleanup did not publish a new scan');
    await writeFile(join(evidence, 'compact-refusal.json'), JSON.stringify(outcome, null, 2));
    cleanupEvidence = 'compact profile safely refused; fixture artifact removed by the test, with a real advanced MCP rescan';
    await rm(artifact);
    await scan(advanced, repo);
  } else {
    assert.equal(outcome.deletedAllocatedBytes, artifactBytes, JSON.stringify(outcome));
    assert.equal(outcome.completion.nextAction, 'report_result');
    await waitFor(async () => {
      const state = (await cleanup('cleanup_status', { operationId, waitMs: 0 })).verification;
      assert(!['deferred', 'failed', 'cancelled'].includes(state?.state), JSON.stringify(state));
      return state?.state === 'complete' && state.fresh;
    }, 'durable MCP scan publication');
  }
  await assert.rejects(lstat(artifact), { code: 'ENOENT' });
  assert.deepEqual(await readFile(source), sourceBytes);
  assert.equal(git('rev-parse', 'HEAD'), gitHead);
  assert.equal(git('status', '--porcelain'), '');
  const finalAllocation = await allocation(repo);
  const beforeRead = await fingerprint();
  await dialog().getByRole('button', { name: '刷新记录' }).click();
  await dialog().getByLabel('MCP 占用变化').waitFor();
  const latest = await readHistory();
  const after = latest.scopes.find(scope => scope.id === latest.selectedScopeId);
  assert.deepEqual(after.roots, [repo]);
  assert.equal(after.allocatedBytes, finalAllocation);
  assert.deepEqual(latest.comparison.baseline, reference.current);
  assert.equal(latest.comparison.reductionBytes, artifactBytes);
  assert.equal(latest.comparison.netBytes, finalAllocation - initialAllocation);
  assert.deepEqual(await fingerprint(), beforeRead, 'Reading did not edit or retouch MCP archives');
  await screenshot('04-cleanup-comparison');

  step = 'other scope stays independent';
  const selector = dialog().getByRole('combobox', { name: 'MCP 扫描范围' });
  await selector.click();
  await page.getByRole('option', { name: /^Other repo/ }).click();
  await dialog().getByText(sibling, { exact: true }).waitFor();
  await dialog().getByText('同一范围还没有可比的上一次扫描。').waitFor();
  await selector.click();
  await page.getByRole('option', { name: /^Sample repo.*跳过受保护目录/ }).click();
  await dialog().getByLabel('MCP 占用变化').waitFor();

  step = 'read failure and recovery';
  const displaced = `${agentCache}.backup`;
  await rename(agentCache, displaced);
  await writeFile(agentCache, 'owned invalid cache path');
  await dialog().getByRole('button', { name: '刷新记录' }).click();
  await dialog().getByRole('alert').waitFor();
  await screenshot('05-read-failure');
  await rm(agentCache);
  await rename(displaced, agentCache);
  await dialog().getByRole('button', { name: '重试' }).click();
  await dialog().getByRole('alert').waitFor({ state: 'hidden' });
  await dialog().getByRole('button', { name: '刷新记录' }).waitFor();
  await dialog().getByLabel('MCP 占用变化').waitFor();
  await screenshot('06-retry');

  step = 'relaunch and read the same retained comparison';
  await app.close();
  await launch();
  await button('MCP 扫描记录').click();
  await dialog().getByLabel('MCP 占用变化').waitFor();
  assert.deepEqual(await readHistory(), latest);
  await screenshot('07-relaunch');
  assert.deepEqual(pageErrors, []);
  result = { status: 'passed', identity: 'development Electron + debug Rust + real MCP stdio, synthetic repos', cleanupEvidence, screenshots };
} catch (error) {
  result = { status: 'failed', step, error: String(error), screenshots, pageErrors };
  if (page) await screenshot('failed').catch(() => {});
  throw error;
} finally {
  console.log(`Evidence: ${evidence}`);
  await writeFile(join(evidence, 'result.json'), JSON.stringify(result, null, 2));
  if (app) await app.close().catch(() => {});
  for (const client of clients) client.stdin.end();
  await rm(fixture, { recursive: true, force: true });
}
