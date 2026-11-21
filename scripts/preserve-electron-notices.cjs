const { copyFile, mkdir, readFile, writeFile } = require('node:fs/promises');
const { join } = require('node:path');
const { Arch } = require('builder-util');

// afterExtract runs before electron-builder's macOS renaming removes the ZIP's
// root license files. These are the actual target SDK bytes, never host fallback.
module.exports = async function preserveElectronNotices(context) {
  if (context.electronPlatformName !== 'darwin') throw new Error('Electron notice hook supports macOS packaging only');
  const arch = Arch[context.arch];
  if (!['arm64', 'x64'].includes(arch)) throw new Error('Unsupported Electron notice architecture');
  const output = join(context.packager.projectDir, '.build', arch, 'licenses');
  // Require the generated application/dependency gate, even for direct builder.
  await readFile(join(output, 'README.txt'), 'utf8');
  await mkdir(output, { recursive: true });
  for (const [source, destination] of [['LICENSE', 'Electron-LICENSE.txt'], ['LICENSES.chromium.html', 'LICENSES.chromium.html']]) {
    const input = join(context.appOutDir, source);
    if (!(await readFile(input, 'utf8')).trim()) throw new Error(`Empty extracted Electron notice: ${source}`);
    await copyFile(input, join(output, destination));
  }
  await writeFile(join(output, 'Electron-SDK.json'), JSON.stringify({ version: context.packager.info.framework.version, platform: 'darwin', arch }, null, 2) + '\n');
};
