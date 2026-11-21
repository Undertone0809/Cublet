import test from 'node:test';
import assert from 'node:assert/strict';
import { createStatusAction } from './status-action.ts';

test('status action reports loading before IPC resolves and blocks duplicate clicks', async () => {
  const events = []; let resolve; let calls = 0;
  const run = createStatusAction((state) => events.push(state));
  const ipc = () => { calls++; return new Promise((r) => { resolve = r; }); };
  const first = run(ipc, 1000);
  assert.equal(events[0].phase, 'pending');
  await run(ipc, 1000); assert.equal(calls, 1);
  resolve('still running'); await first;
  assert.deepEqual(events.map((e) => e.phase), ['pending', 'success']);
  assert.equal(events[1].message, 'still running');
});
test('timeout is explicit and releases controls; late IPC cannot overwrite it', async () => {
  const events = []; let resolve;
  const run = createStatusAction((s) => events.push(s));
  await run(() => new Promise((r) => { resolve = r; }), 5);
  assert.equal(events.at(-1).phase, 'error'); assert.match(events.at(-1).message, /timed out/i);
  resolve('late success'); await new Promise((r) => setTimeout(r, 10));
  assert.equal(events.at(-1).phase, 'error');
  await run(async () => 'closed', 100); assert.equal(events.at(-1).message, 'closed');
});
test('native failure surfaces its message', async () => {
  const events = []; const run = createStatusAction((s) => events.push(s));
  await run(async () => { throw new Error('Permission denied'); }, 100);
  assert.equal(events.at(-1).phase, 'error'); assert.equal(events.at(-1).message, 'Permission denied');
});
