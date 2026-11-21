const { test } = require('node:test');
const assert = require('node:assert/strict');
const { Lazy } = require('./updater-lazy.cjs');

test('defers work and shares the exact pending or fulfilled promise', async () => {
  let calls = 0;
  let finish;
  const operation = new Promise(resolve => { finish = resolve; });
  const deferred = new Lazy(() => { calls++; return operation; });
  assert.equal(calls, 0);
  assert.equal(deferred.value, operation);
  assert.equal(deferred.value, operation);
  assert.equal(calls, 1);
  finish('config');
  assert.equal(await deferred.value, 'config');
  assert.equal(deferred.value, operation);
});

test('retains asynchronous failure until its owner replaces the lazy read', async () => {
  const error = new Error('config unavailable');
  let calls = 0;
  const deferred = new Lazy(() => { calls++; return Promise.reject(error); });
  const operation = deferred.value;
  await assert.rejects(operation, value => value === error);
  assert.equal(deferred.value, operation);
  assert.equal(calls, 1);
});

test('retries synchronous factory throws and caches nullish/falsy results', () => {
  let calls = 0;
  const deferred = new Lazy(() => { if (++calls === 1) throw new Error('retry'); return 0; });
  assert.throws(() => deferred.value, /retry/);
  assert.equal(deferred.value, 0);
  assert.equal(deferred.value, 0);
  assert.equal(calls, 2);
  for (const value of [null, undefined, false]) {
    let reads = 0;
    const lazy = new Lazy(() => { reads++; return value; });
    assert.equal(lazy.value, value);
    assert.equal(lazy.value, value);
    assert.equal(reads, 1);
  }
});
