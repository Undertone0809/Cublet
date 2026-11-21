import assert from 'node:assert/strict';
import test from 'node:test';
import { applyTheme, parseStoredTheme, prefersDarkTheme, readStoredTheme, resolveTheme, THEME_STORAGE_KEY } from './theme.ts';

test('missing and invalid preferences follow the system; old manual choices survive', () => {
  for (const value of ['system', null, '', 'auto', 'unknown']) assert.equal(parseStoredTheme(value), 'system');
  assert.equal(parseStoredTheme('light'), 'light');
  assert.equal(parseStoredTheme('dark'), 'dark');
});

test('system responds to appearance changes while explicit overrides remain stable', () => {
  for (const dark of [false, true, false, true]) {
    assert.equal(resolveTheme('system', dark), dark ? 'dark' : 'light');
    assert.equal(resolveTheme(null, dark), dark ? 'dark' : 'light');
    assert.equal(resolveTheme('light', dark), 'light');
    assert.equal(resolveTheme('dark', dark), 'dark');
  }
});

test('storage returns the preference without resolving or overwriting legacy values', () => {
  const previous = globalThis.window;
  try {
    for (const saved of [null, 'invalid', 'system', 'light', 'dark']) {
      globalThis.window = {
        localStorage: { getItem: key => { assert.equal(key, THEME_STORAGE_KEY); return saved; } },
        matchMedia: () => { throw Error('stored preference must not depend on OS appearance'); },
      };
      assert.equal(readStoredTheme(), parseStoredTheme(saved));
    }
    globalThis.window = { localStorage: { getItem: () => { throw Error('denied'); } } };
    assert.equal(readStoredTheme(), 'system');
    delete globalThis.window;
    assert.equal(readStoredTheme(), 'system');
  } finally {
    if (previous === undefined) delete globalThis.window;
    else globalThis.window = previous;
  }
});

test('resolved appearance applies matching DOM class, color scheme and data attribute', () => {
  const previousWindow = globalThis.window;
  const previousDocument = globalThis.document;
  const classes = new Set();
  const root = { classList: { toggle: (name, on) => on ? classes.add(name) : classes.delete(name) }, dataset: {}, style: {} };
  try {
    globalThis.document = { documentElement: root };
    for (const dark of [true, false, true]) {
      globalThis.window = { matchMedia: query => { assert.equal(query, '(prefers-color-scheme: dark)'); return { matches: dark }; } };
      const resolved = resolveTheme('system', prefersDarkTheme());
      applyTheme(resolved);
      assert.equal(classes.has('dark'), dark);
      assert.equal(root.dataset.theme, resolved);
      assert.equal(root.style.colorScheme, resolved);
    }
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
  }
});
