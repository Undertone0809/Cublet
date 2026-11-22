'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { test } = require('node:test');
const { createApplicationDiscovery, isStandaloneApplicationCandidate } = require('./application-discovery.cjs');

async function fixture(t, overrides = {}) {
  const root = await fs.mkdtemp(path.join(await fs.realpath(os.tmpdir()), 'application-discovery-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  const applicationsRoot = path.join(root, 'Applications');
  const userApplications = path.join(home, 'Applications');
  await Promise.all([applicationsRoot, userApplications].map(value => fs.mkdir(value, { recursive: true })));
  const calls = [];
  const options = {
    home, applicationsRoot,
    plist: async value => JSON.parse(await fs.readFile(value, 'utf8')),
    indexedApplications: async query => { calls.push(query); return []; },
    ...overrides,
  };
  return { root, home, applicationsRoot, userApplications, calls, options };
}

async function makeApp(target, overrides = {}) {
  const executable = path.join(target, 'Contents', 'MacOS', 'Fixture');
  await fs.mkdir(path.dirname(executable), { recursive: true });
  await fs.writeFile(executable, '#!/bin/sh\nexit 0\n', { mode: 0o755 });
  await fs.writeFile(path.join(target, 'Contents', 'Info.plist'), JSON.stringify({
    CFBundlePackageType: 'APPL', CFBundleExecutable: 'Fixture',
    CFBundleIdentifier: 'com.example.fixture', CFBundleDisplayName: 'Fixture',
    CFBundleShortVersionString: '1.2.3', ...overrides,
  }));
  return target;
}

test('discovers both application roots before disk scanning, with explicitly unknown measurements', async t => {
  const f = await fixture(t);
  const first = await makeApp(path.join(f.applicationsRoot, 'Vendor', 'First.app'), { CFBundleDisplayName: 'First' });
  const second = await makeApp(path.join(f.userApplications, 'Second.app'), { CFBundleDisplayName: 'Second' });
  const result = await createApplicationDiscovery(f.options).search();
  assert.deepEqual(result.applications.map(row => row.path), [first, second]);
  assert.equal(result.partial, false);
  assert.deepEqual(f.calls, ['']);
  assert.deepEqual(result.applications[0], {
    path: first, name: 'First', version: '1.2.3', bundleId: 'com.example.fixture',
    bundleBytes: 0, supportBytes: 0, bundleBytesKnown: false,
    measurement: 'unknown', sharedData: false,
  });
});

test('Spotlight supplements default listing and name, path, and identifier searches outside the roots', async t => {
  const f = await fixture(t);
  const outside = await makeApp(path.join(f.root, 'Elsewhere', 'Renamed.app'), {
    CFBundleDisplayName: 'Résumé Editor', CFBundleIdentifier: 'org.example.editor',
  });
  f.options.indexedApplications = async query => { f.calls.push(query); return [outside, outside]; };
  const service = createApplicationDiscovery(f.options);
  for (const query of ['', 'résumé', 'resume', 'renamed', 'org.example.editor']) {
    const result = await service.search({ query });
    assert.deepEqual(result.applications.map(row => row.path), [outside]);
  }
  assert.deepEqual(f.calls, ['', 'résumé', 'resume', 'renamed', 'org.example.editor']);
  assert.equal((await service.search({ query: 'not found' })).applications.length, 0);
});

test('validates APPL and executable; excludes nested packages, helpers, Finder aliases and symlink ancestors', async t => {
  const f = await fixture(t);
  const good = await makeApp(path.join(f.applicationsRoot, 'Good.app'));
  const backup = await makeApp(path.join(f.applicationsRoot, 'Good.app.pre-20261121'));
  const nested = await makeApp(path.join(good, 'Contents', 'Helpers', 'Nested.app'));
  const packaged = await makeApp(path.join(f.applicationsRoot, 'Plugin.bundle', 'Nested.app'));
  const helper = await makeApp(path.join(f.applicationsRoot, 'Vendor Helper (GPU).app'));
  const metadataHelper = await makeApp(path.join(f.applicationsRoot, 'Renamed.app'), { CFBundleDisplayName: 'Vendor Helper' });
  const badType = await makeApp(path.join(f.applicationsRoot, 'Bad.app'), { CFBundlePackageType: 'BNDL' });
  const noExec = await makeApp(path.join(f.applicationsRoot, 'Missing.app'));
  await fs.unlink(path.join(noExec, 'Contents', 'MacOS', 'Fixture'));
  const noMode = await makeApp(path.join(f.applicationsRoot, 'NoMode.app'));
  await fs.chmod(path.join(noMode, 'Contents', 'MacOS', 'Fixture'), 0o644);
  const outside = await makeApp(path.join(f.root, 'External', 'External.app'));
  const linkedDirectory = path.join(f.applicationsRoot, 'Linked');
  await fs.symlink(path.dirname(outside), linkedDirectory);
  const linked = path.join(linkedDirectory, 'External.app');
  const symlink = path.join(f.applicationsRoot, 'Alias.app');
  await fs.symlink(outside, symlink);
  const alias = path.join(f.applicationsRoot, 'Finder Alias.app');
  await fs.writeFile(alias, 'synthetic alias');
  f.options.indexedApplications = async () => [nested, packaged, helper, metadataHelper, badType, noExec, noMode, linked, symlink, alias];
  const result = await createApplicationDiscovery(f.options).search();
  assert.deepEqual(result.applications.map(row => row.path).sort(), [good, backup].sort());
  assert.equal(result.partial, false);
});

test('preserves found rows and marks bounded traversal partial without starving the other root', async t => {
  const f = await fixture(t, { maxEntriesPerRoot: 2 });
  await makeApp(path.join(f.applicationsRoot, 'First.app'));
  await makeApp(path.join(f.applicationsRoot, 'Second.app'));
  await makeApp(path.join(f.applicationsRoot, 'Third.app'));
  const user = await makeApp(path.join(f.userApplications, 'User.app'));
  const result = await createApplicationDiscovery(f.options).search();
  assert.equal(result.partial, true);
  assert.ok(result.applications.some(row => row.path === user));
  assert.ok(result.applications.length >= 2);
  assert.ok(result.notices.some(notice => notice.includes('上限')));
});

test('skips irrelevant build trees and package descendants, and reports depth truncation', async t => {
  const f = await fixture(t, { maxDepth: 1 });
  const visible = await makeApp(path.join(f.applicationsRoot, 'Vendor', 'Visible.app'));
  await makeApp(path.join(f.applicationsRoot, 'build', 'Build.app'));
  await makeApp(path.join(f.applicationsRoot, 'node_modules', 'Dependency.app'));
  await makeApp(path.join(f.applicationsRoot, 'Vendor', 'Deeper', 'Deep.app'));
  const result = await createApplicationDiscovery(f.options).search();
  assert.deepEqual(result.applications.map(row => row.path), [visible]);
  assert.equal(result.partial, true);
  assert.ok(result.notices.some(notice => notice.includes('深度')));
});

test('Spotlight failure retains directory results and enables refresh recovery', async t => {
  const f = await fixture(t);
  const good = await makeApp(path.join(f.applicationsRoot, 'Good.app'));
  let failing = true;
  f.options.indexedApplications = async () => { if (failing) throw new Error('unavailable'); return []; };
  const service = createApplicationDiscovery(f.options);
  const result = await service.search();
  assert.deepEqual(result.applications.map(row => row.path), [good]);
  assert.equal(result.partial, true);
  assert.ok(result.notices.some(notice => notice.includes('Spotlight')));
  failing = false;
  assert.equal((await service.search({ refresh: true })).partial, false);
});

test('query cache coalesces requests, returns isolated results and refreshes new apps', async t => {
  let time = 0;
  const f = await fixture(t, { now: () => time });
  await makeApp(path.join(f.applicationsRoot, 'First.app'), { CFBundleDisplayName: 'First' });
  const service = createApplicationDiscovery(f.options);
  const [a, b] = await Promise.all([service.search(), service.search()]);
  assert.equal(f.calls.length, 1);
  a.applications[0].name = 'mutated';
  assert.equal(b.applications[0].name, 'First');
  await makeApp(path.join(f.applicationsRoot, 'Second.app'), { CFBundleDisplayName: 'Second' });
  assert.equal((await service.search()).applications.length, 1);
  assert.equal((await service.search({ refresh: true })).applications.length, 2);
  await makeApp(path.join(f.applicationsRoot, 'Third.app'), { CFBundleDisplayName: 'Third' });
  time = 15_001;
  assert.equal((await service.search()).applications.length, 3);
});

test('typing a new query reuses validated metadata while refresh revalidates a changed bundle', async t => {
  const f = await fixture(t);
  const target = await makeApp(path.join(f.applicationsRoot, 'First.app'), { CFBundleDisplayName: 'First' });
  let reads = 0;
  f.options.plist = async value => { reads++; return JSON.parse(await fs.readFile(value, 'utf8')); };
  const service = createApplicationDiscovery(f.options);
  await service.search();
  await service.search({ query: 'fir' });
  await service.search({ query: 'first' });
  assert.equal(reads, 1);
  await fs.unlink(path.join(target, 'Contents', 'MacOS', 'Fixture'));
  assert.equal((await service.search({ refresh: true })).applications.length, 0);
  assert.equal(reads, 2);
});

test('result cap is explicit and malformed search/path input cannot reach filesystem or command', async t => {
  const f = await fixture(t, { maxResults: 1 });
  await makeApp(path.join(f.applicationsRoot, 'First.app'));
  await makeApp(path.join(f.applicationsRoot, 'Second.app'));
  const service = createApplicationDiscovery(f.options);
  const result = await service.search();
  assert.equal(result.applications.length, 1);
  assert.equal(result.partial, true);
  await assert.rejects(service.search({ query: '\u0000' }), /无效/);
  assert.equal(isStandaloneApplicationCandidate('/Applications/../Escape.app'), false);
  assert.equal(isStandaloneApplicationCandidate('/Applications/Parent.app.pre-123/Child.app'), false);
});
