import { execFileSync } from 'node:child_process';
import { cp, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { build } from 'vite';
import { renderRuntimeNotices } from './runtime-notices.mjs';

const run = (root, command, args) => execFileSync(command, args, { cwd: root, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 }).trim();
const noticeName = /^(?:licen[cs]es?|copying|copyright|notice|unlicense)(?:$|[._-])/i;
const grantName = /^(?:licen[cs]es?|copying|unlicense)(?:$|[._-])/i;
const divider = '\n\n' + '='.repeat(78) + '\n\n';

async function cssPackageInput(importer, specifier) {
  const require = createRequire(importer);
  const name = specifier.startsWith('@') ? specifier.split('/').slice(0, 2).join('/') : specifier.split('/')[0];
  // CSS-only packages may export only the `style` condition. Locate the owning
  // manifest in the importer's node_modules search path, not a JS entry point.
  for (const directory of require.resolve.paths(name) ?? []) {
    const manifest = join(directory, name, 'package.json');
    try {
      if (JSON.parse(await readFile(manifest, 'utf8')).name === name) return manifest;
    } catch (error) { if (error.code !== 'ENOENT') throw error; }
  }
  throw new Error(`Cannot identify bundled CSS package ${name}`);
}

/** Build the actual renderer once and inventory rendered JS plus its CSS inputs. */
export async function buildRendererNotices({ root }) {
  const modules = new Set();
  const result = await build({ root, plugins: [{
    name: 'cublet-distribution-license-inputs',
    generateBundle() { for (const id of this.getModuleIds()) modules.add(id); },
  }] });
  const inputs = new Set();
  for (const output of Array.isArray(result) ? result : [result]) {
    for (const chunk of output.output) {
      if (chunk.type !== 'chunk') continue;
      for (const [id, info] of Object.entries(chunk.modules)) {
        // Rollup's generated CommonJS proxies are not additional source files.
        if (info.renderedLength > 0 && !id.includes('\0') && id.includes('/node_modules/')) inputs.add(id.split('?')[0]);
      }
    }
  }
  for (const id of modules) {
    const file = id.split('?')[0];
    if (!isAbsolute(file) || !file.endsWith('.css')) continue;
    if (file.includes('/node_modules/')) inputs.add(file);
    else {
      // Tailwind inlines package @imports before Rollup emits CSS modules.
      const css = await readFile(file, 'utf8');
      for (const match of css.matchAll(/@import\s+(?:url\(\s*)?["']([^"']+)["']/g)) {
        const specifier = match[1];
        if (/^(?:\.|\/|https?:|data:)/.test(specifier)) continue;
        inputs.add(await cssPackageInput(file, specifier));
      }
    }
  }
  const notices = await renderRuntimeNotices({ root, inputs: [...inputs], movaFiles: [], strict: true });
  return { ...notices, text: 'Cublet renderer third-party notices\nGenerated from rendered Vite JavaScript modules and resolved CSS/font inputs. Upstream notice files are retained in full.' + divider + notices.text.split(divider).slice(1).join(divider) };
}

async function retainedNotices(directory, explicitLicense) {
  const explicitLocal = explicitLicense ? relative(directory, resolve(directory, explicitLicense)) : null;
  const files = new Set(explicitLocal ? [explicitLocal] : []);
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (!noticeName.test(entry.name)) continue;
    if (entry.isFile()) files.add(entry.name);
    else if (entry.isDirectory()) {
      for (const child of await readdir(join(directory, entry.name), { withFileTypes: true })) {
        if (child.isFile()) files.add(join(entry.name, child.name));
      }
    }
  }
  const notices = [];
  for (const file of [...files].sort()) {
    const local = relative(directory, resolve(directory, file));
    if (local.startsWith('..') || isAbsolute(local)) throw new Error('Crate notice escapes package');
    notices.push({ file, text: (await readFile(join(directory, file), 'utf8')).trimEnd() });
  }
  if (!notices.some(item => (item.file === explicitLocal || grantName.test(item.file)) && item.text.trim())) {
    throw new Error(`Missing full license notice in crate ${directory.split('/').at(-1)}`);
  }
  return notices;
}

/** Cargo metadata includes build dependencies conservatively, filtered by target. */
export async function stageDistributionNotices({ root, arch, target, renderer }) {
  if ({ arm64: 'aarch64-apple-darwin', x64: 'x86_64-apple-darwin' }[arch] !== target || !target) throw new Error('Unsupported notice target/architecture');
  const output = join(root, '.build', arch, 'licenses');
  await rm(output, { recursive: true, force: true });
  await mkdir(output, { recursive: true });
  const appLicense = await readFile(join(root, 'LICENSE'), 'utf8');
  if (!appLicense.includes('Permission is hereby granted')) throw new Error('Missing Cublet MIT license');
  await writeFile(join(output, 'Cublet-LICENSE.txt'), appLicense);
  await writeFile(join(output, 'renderer-THIRD_PARTY_NOTICES.txt'), renderer.text);
  await cp(join(root, 'electron-runtime/THIRD_PARTY_NOTICES.txt'), join(output, 'updater-and-Mova-THIRD_PARTY_NOTICES.txt'));

  const metadata = JSON.parse(run(root, 'cargo', ['metadata', '--locked', '--filter-platform', target, '--format-version', '1', '--manifest-path', 'rust-core/Cargo.toml']));
  const ids = new Set(metadata.resolve.nodes.map(node => node.id));
  const packages = metadata.packages.filter(pkg => pkg.source && ids.has(pkg.id)).sort((a, b) => `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`));
  const sections = [`Cublet Rust sidecar dependency notices\nTarget: ${target}\nIncludes target-resolved runtime and build dependencies from Cargo.lock.`];
  for (const pkg of packages) {
    const directory = dirname(pkg.manifest_path);
    const notices = await retainedNotices(directory, pkg.license_file);
    sections.push([`Package: ${pkg.name}@${pkg.version}`, `Declared license: ${pkg.license ?? 'See license file'}`, `Source: ${pkg.repository ?? pkg.source}`,
      ...notices.map(item => `\n--- ${item.file} ---\n${item.text}`)].join('\n'));
    if (pkg.license?.includes('MPL-2.0')) {
      // Ship the exact, unmodified registry source with the executable notices.
      if (!pkg.source.startsWith('registry+')) throw new Error('MPL source requires a reviewed source origin');
      const sourcePath = `sources/${pkg.name}-${pkg.version}`;
      await cp(directory, join(output, sourcePath), { recursive: true });
      sections.push(`${pkg.name}@${pkg.version} MPL source availability\nThe unmodified source, including its license, is provided in ${sourcePath}/ beside these notices.\nPublished archive: https://crates.io/api/v1/crates/${pkg.name}/${pkg.version}/download`);
    }
    if (pkg.name === 'libsqlite3-sys') {
      const amalgamation = await readFile(join(directory, 'sqlite3/sqlite3.c'), 'utf8');
      const version = amalgamation.match(/SQLite\s*\n\*\* version (\d+\.\d+\.\d+)/)?.[1];
      const disclaimer = amalgamation.match(/\/\*\n\*\* 2001 September 15[\s\S]*?\*\//)?.[0];
      if (!version || !disclaimer?.includes('disclaims copyright')) throw new Error('Missing bundled SQLite public-domain notice');
      sections.push(`SQLite ${version}, bundled by libsqlite3-sys@${pkg.version}\nSource: https://sqlite.org/\nRetained from sqlite3/sqlite3.c:\n\n${disclaimer}`);
    }
  }
  const text = sections.join(divider) + '\n';
  if (text.includes(root) || /\/Users\//.test(text)) throw new Error('Private path in generated crate notices');
  await writeFile(join(output, 'rust-THIRD_PARTY_NOTICES.txt'), text);
  const rustc = process.env.RUSTC || 'rustc';
  const sysroot = run(root, rustc, ['--print', 'sysroot']);
  const rustDocs = join(sysroot, 'share/doc/rust');
  // rust-docs supplies the standard-library component inventory and full texts.
  // Fail closed if this selected toolchain lacks them; install rust-docs to build.
  await cp(join(rustDocs, 'COPYRIGHT-library.html'), join(output, 'rust-COPYRIGHT-library.html'));
  await cp(join(rustDocs, 'licenses'), join(output, 'licenses'), { recursive: true });
  await writeFile(join(output, 'README.txt'), `Cublet distribution licenses\n\nCublet-LICENSE.txt: Cublet-owned code (MIT).\nrenderer-THIRD_PARTY_NOTICES.txt: rendered JavaScript, CSS and font dependencies.\nupdater-and-Mova-THIRD_PARTY_NOTICES.txt: bundled updater and retained Mova notices.\nrust-THIRD_PARTY_NOTICES.txt: ${packages.length} target-resolved crate dependencies, bundled SQLite and MPL source availability.\nrust-COPYRIGHT-library.html and licenses/: selected Rust toolchain library notices.\nElectron-LICENSE.txt and LICENSES.chromium.html: copied by afterExtract from the exact Electron SDK used for this architecture.\n\nRust toolchain: ${run(root, rustc, ['--version'])}\n`);
  console.log(`Staged distribution notices: ${renderer.packages.length} renderer packages, ${packages.length} Rust crates, SQLite, Rust library and Cublet MIT.`);
  return { output, rendererPackages: renderer.packages.length, rustPackages: packages.length };
}

/** Verify every staged byte reached the final app before any distribution. */
export async function verifyDistributionNotices({ root, arch, resources }) {
  const staged = join(root, '.build', arch, 'licenses');
  let count = 0;
  async function verify(directory, prefix = '') {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const local = join(prefix, entry.name);
      if (entry.isDirectory()) await verify(join(directory, entry.name), local);
      else if (entry.isFile()) {
        const [source, delivered] = await Promise.all([readFile(join(staged, local)), readFile(join(resources, 'licenses', local))]);
        if (!source.equals(delivered)) throw new Error(`Packaged notice mismatch: ${local}`);
        count++;
      } else throw new Error(`Unsupported notice file type: ${local}`);
    }
  }
  await verify(staged);
  const sdk = JSON.parse(await readFile(join(staged, 'Electron-SDK.json'), 'utf8'));
  if (sdk.arch !== arch || sdk.platform !== 'darwin') throw new Error('Packaged Electron notice target mismatch');
  console.log(`Verified ${count} license/source files in the final app.`);
}
