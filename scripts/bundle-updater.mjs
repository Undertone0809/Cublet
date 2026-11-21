import { build } from 'esbuild';
import { createRequire } from 'node:module';
import { resolve, join } from 'node:path';
import { generateRuntimeNotices } from './runtime-notices.mjs';

/** Bundle the pinned updater while replacing only its lazy-value dependency.
 * Keep the original npm dependency for upstream/build-tool installation; no
 * lazy-val source may enter the distributed updater runtime.
 */
export async function bundleUpdater({ root, outputDirectory = 'electron-runtime' }) {
  root = resolve(root);
  const require = createRequire(join(root, 'package.json'));
  if (require('electron-updater/package.json').version !== '6.8.11') {
    throw new Error('Audit the first-party lazy adapter contract before changing electron-updater@6.8.11');
  }
  const output = resolve(root, outputDirectory);
  const bundled = await build({
    absWorkingDir: root,
    metafile: true,
    entryPoints: [require.resolve('electron-updater')],
    outfile: join(output, 'updater.cjs'),
    bundle: true,
    platform: 'node',
    format: 'cjs',
    target: 'node22',
    external: ['electron'],
    legalComments: 'eof',
    alias: { 'lazy-val': join(root, 'electron/updater-lazy.cjs') },
  });
  const externalLazy = Object.values(bundled.metafile.outputs).some(output => output.imports.some(input => input.external && /^lazy-val(?:\/|$)/.test(input.path)));
  if (externalLazy || Object.keys(bundled.metafile.inputs).some(input => /(?:^|\/)node_modules\/lazy-val\//.test(input))) {
    throw new Error('Original lazy-val source unexpectedly entered the updater bundle');
  }
  const notices = await generateRuntimeNotices({ root, inputs: bundled.metafile.inputs, output: join(output, 'THIRD_PARTY_NOTICES.txt'), strict: true });
  return { ...bundled, notices };
}
