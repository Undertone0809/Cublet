const fs = require('node:fs/promises');
const path = require('node:path');
const { randomUUID } = require('node:crypto');
const { isApplicationPath } = require('./app-icons.cjs');
const { applicationDiskPath } = require('./app-uninstall.cjs');

const MAX_PATHS = 256;
const MAX_BYTES = 2 * 1024 * 1024;

function createUninstallInventory(app) {
  let state;
  let writing = Promise.resolve();
  const markerFile = () => path.join(app.getPath('userData'), 'trashed-application-paths.json');

  function readState() {
    return state ??= (async () => {
      const empty = { paths: new Set(), movedAtMs: 0 };
      try {
        const file = markerFile();
        const metadata = await fs.lstat(file);
        if (!metadata.isFile() || metadata.size > MAX_BYTES) return empty;
        const stored = JSON.parse(await fs.readFile(file, 'utf8'));
        return {
          paths: new Set(Array.isArray(stored.paths) ? stored.paths.filter(isApplicationPath).slice(-MAX_PATHS).map(applicationDiskPath) : []),
          movedAtMs: Number.isSafeInteger(stored.movedAtMs) ? stored.movedAtMs : 0,
        };
      } catch {
        return empty;
      }
    })();
  }

  async function remember(movedAppPaths, movedAtMs = Date.now()) {
    const stored = await readState();
    stored.movedAtMs = Math.max(stored.movedAtMs, movedAtMs);
    const known = stored.paths;
    for (const appPath of movedAppPaths.filter(isApplicationPath).map(applicationDiskPath)) {
      known.delete(appPath);
      known.add(appPath);
    }
    while (known.size > MAX_PATHS) known.delete(known.values().next().value);
    writing = writing.catch(() => {}).then(async () => {
      const file = markerFile();
      const temporary = `${file}.${randomUUID()}.tmp`;
      try {
        await fs.mkdir(path.dirname(file), { recursive: true });
        await fs.writeFile(temporary, JSON.stringify({ paths: [...stored.paths], movedAtMs: stored.movedAtMs }), { mode: 0o600, flag: 'wx' });
        await fs.rename(temporary, file);
      } finally {
        await fs.rm(temporary, { force: true }).catch(() => {});
      }
    });
    await writing;
  }

  async function filter(applications) {
    const { paths: known } = await readState();
    if (!known.size) return applications;
    const visible = await Promise.all(applications.map(async (application) => {
      if (!isApplicationPath(application.path) || !known.has(applicationDiskPath(application.path))) return true;
      try {
        await fs.lstat(application.path);
        return true; // Finder Put Back or a reinstall can restore the exact path.
      } catch (error) {
        return error.code !== 'ENOENT' && error.code !== 'ENOTDIR';
      }
    }));
    return applications.filter((_, index) => visible[index]);
  }

  async function isStale(startedAtMs) {
    const { movedAtMs } = await readState();
    return typeof startedAtMs === 'number' && startedAtMs <= movedAtMs && movedAtMs > 0;
  }

  return { filter, isStale, remember };
}

module.exports = { createUninstallInventory };
