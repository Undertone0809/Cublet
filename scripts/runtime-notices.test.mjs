import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { generateRuntimeNotices, renderRuntimeNotices } from './runtime-notices.mjs';
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'cublet-notices-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  async function pkg(path, metadata, license) {
    const directory = join(root, 'node_modules', path); await mkdir(directory, { recursive: true });
    await writeFile(join(directory, 'package.json'), JSON.stringify(metadata));
    await writeFile(join(directory, 'index.js'), '');
    if (license) await writeFile(join(directory, 'LICENSE'), license);
    return `node_modules/${path}/index.js`;
  }
  return { root, pkg };
}
test('uses actual bundled packages, deduplicates inputs and retains complete license/NOTICE text', async t => {
  const { root, pkg } = await fixture(t);
  const included = await pkg('@scope/included', { name: '@scope/included', version: '1.2.3', license: 'MIT', author: 'Example', repository: { url: 'https://example.test/source' } }, 'Exact full license\nsecond line\n');
  await writeFile(join(root, 'node_modules/@scope/included/NOTICE.txt'), 'Attribution notice\n');
  await pkg('unused', { name: 'unused', version: '8', license: 'MIT' }, 'Must not appear');
  const result = await generateRuntimeNotices({ root, inputs: [included, included], movaFiles: [], strict: true });
  assert.equal(result.packages.length, 1); assert.deepEqual(result.warnings, []);
  const text = await readFile(result.path, 'utf8');
  assert.match(text, /@scope\/included@1.2.3/); assert.match(text, /Exact full license\nsecond line/); assert.match(text, /Attribution notice/);
  assert.ok(!text.includes(root)); assert.ok(!text.includes('unused'));
});
test('keeps nested installed package versions distinct', async t => {
  const { root, pkg } = await fixture(t);
  const first = await pkg('first', { name: 'same', version: '1', license: 'MIT' }, 'License one');
  const second = await pkg('parent/node_modules/second', { name: 'same', version: '2', license: 'ISC' }, 'License two');
  const result = await renderRuntimeNotices({ root, inputs: { [first]: {}, [second]: {} }, movaFiles: [] });
  assert.equal(result.packages.length, 2); assert.match(result.text, /same@1/); assert.match(result.text, /same@2/);
});
test('candidate mode records missing upstream grants factually; strict mode blocks public export', async t => {
  const { root, pkg } = await fixture(t);
  const input = await pkg('missing', { name: 'missing', version: '1.0.5', license: 'MIT', author: 'Published Author', repository: 'https://example.test/missing' });
  const result = await renderRuntimeNotices({ root, inputs: [input], movaFiles: [] });
  assert.equal(result.warnings.length, 1); assert.match(result.text, /NOTICE GAP/); assert.match(result.text, /Published author: Published Author/);
  assert.ok(!result.text.includes('Permission is hereby granted'));
  await assert.rejects(renderRuntimeNotices({ root, inputs: [input], movaFiles: [], strict: true }), /missing@1.0.5.*no separate license notice/);
});
test('retains Mova text and refuses paths outside the source root', async t => {
  const { root } = await fixture(t); await writeFile(join(root, 'Mova-LICENSE'), 'Mova full original grant');
  const result = await renderRuntimeNotices({ root, inputs: [], movaFiles: ['Mova-LICENSE'] });
  assert.match(result.text, /Mova full original grant/);
  await assert.rejects(renderRuntimeNotices({ root, inputs: ['../node_modules/external/index.js'], movaFiles: [] }), /outside the project/);
});
test('an empty LICENSE cannot borrow nonempty NOTICE attribution to pass strict mode', async t => {
  const { root, pkg } = await fixture(t);
  const input = await pkg('empty', { name: 'empty', version: '1', license: 'MIT' }, '  \n');
  await writeFile(join(root, 'node_modules/empty/NOTICE'), 'Attribution only');
  await assert.rejects(renderRuntimeNotices({ root, inputs: [input], movaFiles: [], strict: true }), /empty@1.*no separate license notice/);
});
