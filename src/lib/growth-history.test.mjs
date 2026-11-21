import assert from 'node:assert/strict';
import test from 'node:test';
import { growthPreferenceFor, localDayKey, localDayStart, readGrowthPreferences, resolveGrowthBaseline, validateGrowthHistory, withGrowthPreference } from './growth-history.ts';

process.env.TZ = 'Asia/Shanghai';
const stamp = (day, hour = 12, minute = 0) => new Date(2026, 10, day, hour, minute).getTime();
const snapshot = (id, time) => ({ id, startedAtMs: time - 100, updatedAtMs: time });
const history = (current, snapshots = [], scopeId = 'scope-A') => ({ scopeId, current, snapshots });

test('five scans today keep the latest complete snapshot before local midnight', () => {
  const yesterday = snapshot('yesterday-last', stamp(13, 23, 59));
  const today = [8, 9, 10, 11, 12].map(hour => snapshot(`today-${hour}`, stamp(14, hour)));
  let choice;
  for (let index = 0; index < today.length; index++) {
    const resolved = resolveGrowthBaseline(history(today[index], [...today.slice(0, index).reverse(), yesterday, snapshot('older', stamp(13, 8))]), choice);
    assert.equal(resolved.baseline.id, yesterday.id);
    assert.equal(resolved.firstOfDay, false);
    assert.equal(resolved.status, 'ready');
    choice = resolved.choice;
  }
});

test('no previous day pins today first current and waits for another endpoint', () => {
  const first = snapshot('today-first', stamp(14, 9));
  const initial = resolveGrowthBaseline(history(first));
  assert.equal(initial.status, 'awaiting-next');
  assert.equal(initial.firstOfDay, true);
  const later = [10, 11, 12, 13].map(hour => snapshot(`later-${hour}`, stamp(14, hour)));
  for (let index = 0; index < later.length; index++) {
    const resolved = resolveGrowthBaseline(history(later[index], [...later.slice(0, index), first]), initial.choice);
    assert.equal(resolved.baseline.id, first.id);
    assert.equal(resolved.status, 'ready');
    assert.equal(resolved.firstOfDay, true);
  }
});

test('fresh daily selection uses the earliest retained same-day record, independent of order', () => {
  const resolved = resolveGrowthBaseline(history(snapshot('end', stamp(14, 17)), [snapshot('mid', stamp(14, 14)), snapshot('first', stamp(14, 10))]));
  assert.equal(resolved.baseline.id, 'first');
  assert.equal(resolved.firstOfDay, true);
});

test('a gap uses the actual earlier record, without inventing a midnight snapshot', () => {
  const old = snapshot('days-ago', stamp(9, 15));
  const resolved = resolveGrowthBaseline(history(snapshot('end', stamp(14, 17)), [snapshot('today', stamp(14, 10)), old]));
  assert.deepEqual(resolved.baseline, old);
  assert.equal(resolved.firstOfDay, false);
});

test('pinned daily and explicit baselines becoming unavailable never slide to a newer record', () => {
  const first = snapshot('first', stamp(14, 9));
  const initial = resolveGrowthBaseline(history(snapshot('end', stamp(14, 10)), [first]));
  const updated = history(snapshot('end-new', stamp(14, 13)), [snapshot('mid', stamp(14, 12))]);
  assert.equal(resolveGrowthBaseline(updated, initial.choice).status, 'baseline-unavailable');
  assert.equal(resolveGrowthBaseline(updated, { mode: 'snapshot', id: first.id, updatedAtMs: first.updatedAtMs }).status, 'baseline-unavailable');
});

test('daily selection advances only when the complete endpoint enters another local day', () => {
  const yesterday = snapshot('before-day', stamp(13, 23));
  const lastToday = snapshot('today-last', stamp(14, 23));
  const initial = resolveGrowthBaseline(history(lastToday, [yesterday]));
  const next = resolveGrowthBaseline(history(snapshot('tomorrow', stamp(15, 0, 1)), [lastToday, yesterday]), initial.choice);
  assert.equal(next.baseline.id, lastToday.id);
  assert.equal(next.choice.dayKey, '2026-11-15');
});

test('previous mode follows the last complete endpoint while explicit mode remains fixed', () => {
  const selected = snapshot('selected', stamp(13, 22));
  const newer = snapshot('newer', stamp(14, 10));
  const records = history(snapshot('end', stamp(14, 12)), [selected, newer]);
  assert.equal(resolveGrowthBaseline(records, { mode: 'previous' }).baseline.id, newer.id);
  assert.equal(resolveGrowthBaseline(records, { mode: 'snapshot', id: selected.id }).baseline.id, selected.id);
});

test('scope-specific explicit selections and daily anchors survive JSON persistence and mode unmounts', () => {
  const initial = readGrowthPreferences(null);
  const day = resolveGrowthBaseline(history(snapshot('first', stamp(14, 8)))).choice;
  const explicit = { mode: 'snapshot', id: 'selected-A', updatedAtMs: stamp(13, 19) };
  const state = withGrowthPreference(withGrowthPreference(initial, 'scope-A', explicit), 'scope-B', day);
  const restored = readGrowthPreferences(JSON.stringify(state));
  assert.deepEqual(growthPreferenceFor(restored, 'scope-A'), explicit);
  assert.deepEqual(growthPreferenceFor(restored, 'scope-B'), day);
  assert.deepEqual(growthPreferenceFor(restored, 'scope-C'), { mode: 'day' });
});

test('corrupt, old-format and unexpected preferences do not become fabricated explicit selections', () => {
  for (const raw of ['broken', '{"mode":"previous"}', '{"version":2,"selections":[]}', '{"version":1,"selections":[{"scopeId":"x","choice":{"mode":"snapshot"}}]}']) {
    assert.deepEqual(readGrowthPreferences(raw), { version: 1, selections: [] });
  }
});

test('history RPC absence/failure differs from a valid catalog with no complete endpoint', () => {
  for (const raw of [undefined, null, {}, { snapshots: [] }, { scopeId: 'A', snapshots: [{ id: 'bad' }] }]) {
    assert.throws(() => validateGrowthHistory(raw), /unavailable/);
  }
  const empty = validateGrowthHistory({ scopeId: 'A', snapshots: [] });
  assert.equal(resolveGrowthBaseline(empty).status, 'no-current');
  assert.equal(resolveGrowthBaseline(history(snapshot('first', stamp(14))), { mode: 'previous' }).status, 'no-baseline');
});

test('day boundaries use local completion dates rather than UTC or scan start dates', () => {
  const current = { id: 'crosses-midnight', startedAtMs: stamp(13, 23, 59), updatedAtMs: stamp(14, 0, 1) };
  const prior = snapshot('local-yesterday', stamp(13, 23, 58));
  const resolved = resolveGrowthBaseline(history(current, [prior]));
  assert.equal(localDayKey(current.updatedAtMs), '2026-11-14');
  assert.equal(resolved.choice.dayKey, '2026-11-14');
  assert.equal(localDayStart(current.updatedAtMs), stamp(14, 0));
  assert.equal(resolved.baseline.id, prior.id);
});

test('local midnight observes daylight-saving boundaries instead of subtracting 24 hours', () => {
  const original = process.env.TZ;
  try {
    process.env.TZ = 'America/New_York';
    const springDay = new Date(2026, 2, 8, 18).getTime();
    const nextDay = new Date(2026, 2, 9, 18).getTime();
    assert.equal(localDayStart(nextDay) - localDayStart(springDay), 23 * 60 * 60 * 1000);
  } finally { process.env.TZ = original; }
});
