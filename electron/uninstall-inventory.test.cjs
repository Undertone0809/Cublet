const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { createUninstallInventory } = require('./uninstall-inventory.cjs');

test('cached applications hide only recorded missing bundles and show restored bundles', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'uninstall-inventory-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const app = { getPath: () => root };
  const moved = { path: path.join(root, 'Moved.app') };
  const unrelated = { path: path.join(root, 'Other.app') };

  await createUninstallInventory(app).remember([moved.path], 1000);
  const afterRestart = createUninstallInventory(app);
  assert.deepEqual(await afterRestart.filter([moved, unrelated]), [unrelated]);
  assert.equal(await afterRestart.isStale(999), true);
  assert.equal(await afterRestart.isStale(1001), false);

  await fs.mkdir(moved.path);
  assert.deepEqual(await afterRestart.filter([moved, unrelated]), [moved, unrelated]);
});
