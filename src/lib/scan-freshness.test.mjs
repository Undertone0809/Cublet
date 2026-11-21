import assert from 'node:assert/strict';
import test from 'node:test';
import { needsScanRefresh, readScanFreshness, recordScanRefresh } from './scan-freshness.ts';

const scan = (scanId, path, startedAtMs, extra = {}) => ({ scanId, roots: [path], startedAtMs, ...extra });
const empty = () => readScanFreshness(null);

test('child refresh expires every ancestor, exact old result and old descendants, but not siblings', () => {
  const state = recordScanRefresh(empty(), scan('new-child', '/fixture/parent/child', 20));
  const broad = scan('broad', '/fixture', 10);
  for (const path of ['/fixture', '/fixture/parent', '/fixture/parent/child', '/fixture/parent/child/deep']) {
    assert.equal(needsScanRefresh(state, broad, path), true, path);
  }
  for (const path of ['/fixture/sibling', '/fixture/parent/childish', '/elsewhere']) {
    assert.equal(needsScanRefresh(state, broad, path), false, path);
  }
  assert.equal(needsScanRefresh(state, broad), true);
  assert.equal(needsScanRefresh(state, { ...broad, roots: ['/other', '/fixture'] }), true);
  assert.equal(needsScanRefresh(state, scan('new-child', '/fixture/parent/child', 20)), false);
});

test('start time controls freshness even when an older broad scan finishes later', () => {
  const state = recordScanRefresh(empty(), scan('refresh', '/fixture/child', 20));
  assert.equal(needsScanRefresh(state, scan('old', '/fixture', 10, { updatedAtMs: 40 })), true);
  assert.equal(needsScanRefresh(state, scan('tie', '/fixture', 20)), true);
  assert.equal(needsScanRefresh(state, scan('fresh', '/fixture', 21)), false);
});

test('cancelled, failed and incomplete refreshes keep ancestors invalidated', () => {
  for (const status of ['running', 'cancelled', 'failed', 'complete']) {
    const state = recordScanRefresh(empty(), scan('refresh', '/fixture/child', 20, { status, coverageComplete: false }));
    assert.equal(needsScanRefresh(state, scan('old', '/fixture', 10)), true, status);
  }
});

test('persisted invalidation survives relaunch and a newer ancestor scan is reusable', () => {
  let state = recordScanRefresh(empty(), scan('child', '/fixture/child', 20));
  state = readScanFreshness(JSON.stringify(state));
  assert.equal(needsScanRefresh(state, scan('old-parent', '/fixture', 10)), true);
  state = recordScanRefresh(state, scan('fresh-parent', '/fixture', 30));
  assert.equal(state.refreshes.length, 1);
  assert.equal(needsScanRefresh(state, scan('fresh-parent', '/fixture', 30)), false);
  assert.equal(needsScanRefresh(state, scan('older-child', '/fixture/child', 20)), true);
  assert.equal(needsScanRefresh(state, scan('fresh-parent', '/fixture', 30), '/fixture/child'), false);
});

test('independent changes remain invalidated and ledger overflow conservatively expires old snapshots', () => {
  let state = empty();
  for (let n = 1; n <= 130; n++) state = recordScanRefresh(state, scan(`scan-${n}`, `/fixture-${n}`, n));
  assert.equal(state.refreshes.length, 128);
  assert.equal(state.beforeMs, 2);
  state = readScanFreshness(JSON.stringify(state));
  assert.equal(needsScanRefresh(state, scan('old', '/fixture-1', 1)), true);
  assert.equal(needsScanRefresh(state, scan('new', '/unrelated', 131)), false);
});

test('malformed stored freshness is ignored', () => {
  for (const value of ['{', 'null', '{}', '{"beforeMs":-1,"refreshes":[]}',
    '{"beforeMs":0,"refreshes":[null]}', '{"beforeMs":0,"refreshes":[{"scanId":"x","startedAtMs":1,"roots":[3]}]}']) {
    assert.deepEqual(readScanFreshness(value), empty());
  }
});
