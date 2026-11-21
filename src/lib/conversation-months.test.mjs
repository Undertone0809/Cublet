import test from 'node:test';
import assert from 'node:assert/strict';
import { monthKey, groupConversations, monthBulkCandidates, toggleSelection } from './conversation-months.ts';

const item = (id, date, status = 'archived') => ({ id, updated_at_ms: date, status, bytes: 1 });
test('month boundaries use local calendar, including leap days and year rollover', () => {
  assert.equal(monthKey(new Date(2026, 0, 1, 0, 0).getTime()), '2026-01');
  assert.equal(monthKey(new Date(2025, 11, 31, 23, 59).getTime()), '2025-12');
  assert.equal(monthKey(new Date(2024, 1, 29, 23, 59).getTime()), '2024-02');
  assert.equal(monthKey(null), null);
  assert.equal(monthKey(NaN), null);
  assert.equal(monthKey(1e30), null);
});
test('groups newest first, places unknown last, and preserves all records', () => {
  const records = [item('unknown', null), item('jan', new Date(2026, 0, 2).getTime()), item('feb', new Date(2026, 1, 2).getTime())];
  assert.deepEqual(groupConversations(records).map(({ key }) => key), ['2026-02', '2026-01', null]);
  assert.equal(groupConversations(records).flatMap(({ items }) => items).length, 3);
});
test('bulk defaults protect unarchived and missing-status records; unknown stays manual', () => {
  const records = [item('a', 0), item('b', 0, 'current'), item('c', null), item('d', 0, 'missing')];
  assert.deepEqual(monthBulkCandidates(records, false).map((row) => row.id), ['a']);
  assert.deepEqual(monthBulkCandidates(records, true).map((row) => row.id), ['a', 'b', 'd']);
});
test('multi-month and filtered selection preserves out-of-filter manual selections', () => {
  const records = [item('a', 0), item('b', 0)];
  const next = toggleSelection(new Set(['manual', 'a']), records);
  assert.deepEqual([...next], ['manual', 'a', 'b']);
  assert.deepEqual([...toggleSelection(next, records)], ['manual']);
});
test('external records are counted in groups but excluded from every bulk deletion mode', () => {
  const records = [item('local', 0), { ...item('external', 0), external: true }];
  assert.equal(groupConversations(records)[0].items.length, 2);
  for (const includeCurrent of [false, true]) assert.deepEqual(monthBulkCandidates(records, includeCurrent).map((row) => row.id), ['local']);
});
