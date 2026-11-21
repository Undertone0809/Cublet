import { readFile, readdir, mkdir, writeFile, rename } from 'node:fs/promises';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const defaultMovaFiles = ['src/components/mova/LICENSE', 'src/components/mova/LICENSE.third-party'];
const noticeName = /^(?:licen[cs]es?|copying|copyright|notice|unlicense)(?:$|[._-])/i;
const grantName = /^(?:licen[cs]es?|copying|unlicense)(?:$|[._-])/i;
const portable = path => path.split(sep).join('/');
const inside = (root, target) => { const path = relative(root, target); return path !== '..' && !path.startsWith('..' + sep) && !path.startsWith(sep); };
const metadataText = value => typeof value === 'string' ? value : value ? JSON.stringify(value) : 'Not declared';
async function readJson(path) { return JSON.parse(await readFile(path, 'utf8')); }
async function packageForInput(root, input) {
  if (input.startsWith('<')) return null;
  const absolute = resolve(root, input);
  if (!inside(root, absolute)) throw new Error(`Bundled input is outside the project: ${input}. Use project-local dependencies.`);
  if (!portable(relative(root, absolute)).split('/').includes('node_modules')) return null;
  let directory = dirname(absolute);
  while (inside(root, directory) && directory !== root) {
    try { return { directory, metadata: await readJson(join(directory, 'package.json')) }; }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
    directory = dirname(directory);
  }
  throw new Error(`Could not identify the package for bundled input ${portable(relative(root, absolute))}`);
}
async function noticeFiles(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (!noticeName.test(entry.name)) continue;
    if (entry.isFile()) files.push(entry.name);
    else if (entry.isDirectory()) {
      for (const child of await readdir(join(directory, entry.name), { withFileTypes: true })) {
        if (child.isFile()) files.push(join(entry.name, child.name));
      }
    }
  }
  return files.sort((a, b) => a.localeCompare(b));
}
/** Render factual notices from the packages that esbuild actually included.
 * Non-strict mode retains metadata-only gaps visibly for private candidates.
 * Strict mode is a future public-export gate, never an inferred license grant.
 */
export async function renderRuntimeNotices({ root, inputs, strict = false, movaFiles = defaultMovaFiles }) {
  root = resolve(root);
  const inputNames = Array.isArray(inputs) ? inputs : Object.keys(inputs ?? {});
  const located = await Promise.all(inputNames.map(input => packageForInput(root, input)));
  const unique = new Map(located.filter(Boolean).map(item => [item.directory, item]));
  const packages = [];
  const warnings = [];
  for (const { directory, metadata } of unique.values()) {
    const name = metadata.name;
    const version = metadata.version;
    if (typeof name !== 'string' || typeof version !== 'string') throw new Error(`Missing package name/version in ${portable(relative(root, directory))}/package.json`);
    const files = await noticeFiles(directory);
    const notices = await Promise.all(files.map(async file => ({ file: portable(file), text: (await readFile(join(directory, file), 'utf8')).trimEnd() })));
    const hasLicense = notices.some(item => grantName.test(item.file.split('/')[0]) && item.text.trim());
    const item = { name, version, declaredLicense: metadataText(metadata.license ?? metadata.licenses), author: metadataText(metadata.author), source: metadataText(metadata.repository?.url ?? metadata.repository ?? metadata.homepage), notices, missingLicenseNotice: !hasLicense };
    if (!hasLicense) warnings.push(`${name}@${version}: upstream package contains no separate license notice. Declared license: ${item.declaredLicense}. Obtain an authoritative version-matched notice before strict public export.`);
    packages.push(item);
  }
  packages.sort((a, b) => `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`));
  warnings.sort();
  if (strict && warnings.length) throw new Error(`Missing bundled license notices:\n${warnings.join('\n')}`);
  const sections = ['Cublet bundled runtime and Mova third-party notices', 'Generated from actual esbuild inputs and retained upstream files. This inventory does not represent a complete product license audit.'];
  for (const item of packages) {
    const lines = [`Package: ${item.name}@${item.version}`, `Declared license: ${item.declaredLicense}`, `Published author: ${item.author}`, `Source: ${item.source}`];
    if (item.missingLicenseNotice) lines.push('NOTICE GAP: Upstream package contains no separate license notice. Metadata above is retained as published; no copyright notice or license grant has been invented.');
    for (const notice of item.notices) lines.push(`\n--- ${notice.file} ---\n${notice.text}`);
    sections.push(lines.join('\n'));
  }
  for (const file of movaFiles) {
    const path = resolve(root, file);
    if (!inside(root, path)) throw new Error('Mova notice must be inside the project');
    const text = (await readFile(path, 'utf8')).trimEnd();
    if (!text.trim()) throw new Error(`Empty retained Mova notice: ${file}`);
    sections.push(`Retained Mova notice: ${portable(relative(root, path))}\n\n${text}`);
  }
  const text = sections.join('\n\n' + '='.repeat(78) + '\n\n') + '\n';
  if (text.includes(root)) throw new Error('Generated notices unexpectedly contain the absolute project path');
  return { text, packages, warnings };
}
export async function generateRuntimeNotices(options) {
  const rendered = await renderRuntimeNotices(options);
  const output = resolve(options.root, options.output ?? 'electron-runtime/THIRD_PARTY_NOTICES.txt');
  await mkdir(dirname(output), { recursive: true });
  const temporary = `${output}.${process.pid}.tmp`;
  await writeFile(temporary, rendered.text, 'utf8');
  await rename(temporary, output);
  return { ...rendered, path: output };
}
// CLI: node scripts/runtime-notices.mjs METAFILE.json [OUTPUT] [--strict]
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2).filter(arg => arg !== '--strict');
  if (!args[0]) throw new Error('Usage: node scripts/runtime-notices.mjs METAFILE.json [OUTPUT] [--strict]');
  const root = fileURLToPath(new URL('..', import.meta.url));
  const result = await generateRuntimeNotices({ root, inputs: (await readJson(resolve(args[0]))).inputs, output: args[1], strict: process.argv.includes('--strict') });
  for (const warning of result.warnings) console.warn(warning);
  console.log(`Wrote ${result.packages.length} bundled package notices to ${portable(relative(root, result.path))}`);
}
