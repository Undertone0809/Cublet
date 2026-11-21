'use strict';

const { posix } = require('node:path');
const fs = require('node:fs/promises');
const { execFile } = require('node:child_process');
const { promisify } = require('node:util');
const runFile = promisify(execFile);
const PNG_SIGNATURE = Buffer.from('89504e470d0a1a0a', 'hex');
const MAX_RESOURCE_BYTES = 8 * 1024 * 1024;

function pngSize(bytes) {
  if (bytes.length < 24 || !bytes.subarray(0, 8).equals(PNG_SIGNATURE) ||
      bytes.toString('ascii', 12, 16) !== 'IHDR') return null;
  const width = bytes.readUInt32BE(16);
  const height = bytes.readUInt32BE(20);
  return width > 0 && height > 0 && width <= 2048 && height <= 2048 ? { width, height } : null;
}

// Electron does not decode .icns directly. Modern bundles contain PNG chunks;
// select the smallest representation at least 64px, otherwise the largest one.
function extractIcnsPng(bytes) {
  if (!Buffer.isBuffer(bytes) || bytes.length < 8 || bytes.length > MAX_RESOURCE_BYTES ||
      bytes.toString('ascii', 0, 4) !== 'icns' || bytes.readUInt32BE(4) !== bytes.length) return null;
  let best = null;
  let bestScore = Infinity;
  for (let offset = 8; offset < bytes.length;) {
    if (offset + 8 > bytes.length) return null;
    const length = bytes.readUInt32BE(offset + 4);
    if (length < 8 || offset + length > bytes.length) return null;
    const chunk = bytes.subarray(offset + 8, offset + length);
    const size = pngSize(chunk);
    if (size) {
      const side = Math.max(size.width, size.height);
      const score = side >= 64 ? side - 64 : 4096 - side;
      if (score < bestScore) { best = chunk; bestScore = score; }
    }
    offset += length;
  }
  return best;
}

function bundleIconNames(plist) {
  const primary = plist?.CFBundleIcons?.CFBundlePrimaryIcon;
  const values = [plist?.CFBundleIconFile, ...(Array.isArray(primary?.CFBundleIconFiles) ? primary.CFBundleIconFiles : []),
    primary?.CFBundleIconName, plist?.CFBundleIconName];
  return [...new Set(values.filter(name => typeof name === 'string' && name.length <= 1024 && name.length > 0 &&
    !name.includes('\0') && !posix.isAbsolute(name) && posix.normalize(name) === name &&
    !name.split('/').some(part => part === '..' || part === '.')))].slice(0, 16);
}

async function readBoundedFile(path, maximum) {
  const handle = await fs.open(path, 'r');
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size <= 0 || stat.size > maximum) return null;
    const bytes = Buffer.alloc(stat.size);
    let offset = 0;
    while (offset < bytes.length) {
      const { bytesRead } = await handle.read(bytes, offset, bytes.length - offset, offset);
      if (!bytesRead) return null;
      offset += bytesRead;
    }
    return bytes;
  } finally { await handle.close(); }
}

async function loadBundleIcon(path, nativeImage) {
  if (typeof nativeImage?.createFromBuffer !== 'function') return null;
  try {
    const bundle = await fs.realpath(path);
    const contents = await fs.realpath(posix.join(bundle, 'Contents'));
    if (!contents.startsWith(bundle + '/')) return null;
    const metadata = await fs.realpath(posix.join(contents, 'Info.plist'));
    if (!metadata.startsWith(contents + '/')) return null;
    const stat = await fs.stat(metadata);
    if (!stat.isFile() || stat.size > 1024 * 1024) return null;
    const { stdout } = await runFile('/usr/bin/plutil', ['-convert', 'json', '-o', '-', metadata],
      { encoding: 'utf8', timeout: 1500, maxBuffer: 1024 * 1024 });
    const names = bundleIconNames(JSON.parse(stdout));
    const resources = await fs.realpath(posix.join(contents, 'Resources'));
    if (!resources.startsWith(contents + '/')) return null;
    for (const name of names) {
      const candidates = posix.extname(name) ? [name] : [name + '.icns', name + '.png'];
      for (const candidate of candidates) {
        try {
          const resource = await fs.realpath(posix.join(resources, candidate));
          if (!resource.startsWith(resources + '/')) continue;
          const bytes = await readBoundedFile(resource, MAX_RESOURCE_BYTES);
          if (!bytes) continue;
          const png = pngSize(bytes) ? bytes : extractIcnsPng(bytes);
          if (!png) continue;
          const icon = nativeImage.createFromBuffer(png);
          if (!icon.isEmpty()) return icon.resize({ width: 64, height: 64, quality: 'best' });
        } catch { /* Try the next declared bundle icon, then the system fallback. */ }
      }
    }
  } catch { /* Missing or unreadable app metadata uses the system fallback. */ }
  return null;
}

const LIMITS = Object.freeze({
  maxBytes: 4 * 1024 * 1024,
  maxEntries: 128,
  maxApplications: 4096,
  maxConcurrent: 4,
  maxPending: 128,
  maxIconBytes: 128 * 1024,
});

function isApplicationPath(path) {
  return typeof path === 'string' && path.length <= 4096 &&
    !path.includes('\0') && posix.isAbsolute(path) && /\.app(?:[.-][^/]+)?$/i.test(path) &&
    !path.split('/').some(part => part === '..' || part === '.') &&
    posix.normalize(path) === path;
}

// Only the main process may register paths, from Rust's applications results.
// All retained icons are data URLs in this bounded in-memory LRU; no disk writes.
function createAppIconStore({ app, nativeImage, ...options }) {
  if (typeof app?.getFileIcon !== 'function') throw new TypeError('app.getFileIcon is required');
  const limits = Object.fromEntries(Object.entries(LIMITS).map(([key, ceiling]) => [
    key, Number.isFinite(options[key]) ? Math.max(1, Math.min(ceiling, Math.floor(options[key]))) : ceiling,
  ]));
  const applications = new Map();
  const cache = new Map();
  const pending = new Map();
  const queue = [];
  let cacheBytes = 0;
  let active = 0;

  function evict(path) {
    const entry = cache.get(path);
    if (!entry) return;
    cacheBytes -= entry.bytes;
    cache.delete(path);
  }

  function rememberApplications(items) {
    if (!Array.isArray(items)) return;
    for (const application of items) {
      const path = application?.path;
      if (!isApplicationPath(path)) continue;
      applications.delete(path);
      applications.set(path, true);
      while (applications.size > limits.maxApplications) {
        const oldest = applications.keys().next().value;
        applications.delete(oldest);
        evict(oldest);
      }
    }
  }

  function retain(path, value) {
    const bytes = 2 * (path.length + (value?.length ?? 0));
    if (bytes > limits.maxBytes || !applications.has(path)) return;
    evict(path);
    while (cache.size >= limits.maxEntries || cacheBytes + bytes > limits.maxBytes) {
      evict(cache.keys().next().value);
    }
    cache.set(path, { value, bytes });
    cacheBytes += bytes;
  }

  async function load(path) {
    if (!applications.has(path)) return null;
    try {
      // On macOS getFileIcon can return the same generic icon for every .app.
      // Prefer the icon declared by this bundle and use the system API as fallback.
      const bundleIcon = await loadBundleIcon(path, nativeImage);
      if (!applications.has(path)) return null;
      const icon = bundleIcon ?? await app.getFileIcon(path, { size: 'normal' });
      if (icon.isEmpty()) return null;
      const { width, height } = icon.getSize();
      // Bound encoding work even if a platform unexpectedly returns a huge image.
      if (!(width > 0 && height > 0 && width <= 256 && height <= 256)) return null;
      const value = icon.toDataURL();
      if (typeof value !== 'string' || value.length * 2 > limits.maxIconBytes ||
          !/^data:image\/png;base64,[A-Za-z0-9+/]+={0,2}$/.test(value)) return null;
      return applications.has(path) ? value : null;
    } catch {
      return null;
    }
  }

  function pump() {
    while (active < limits.maxConcurrent && queue.length) {
      const { path, resolve } = queue.shift();
      active += 1;
      load(path).then(value => {
        // A path may have aged out of the registration bound while loading.
        const result = applications.has(path) ? value : null;
        retain(path, result);
        pending.delete(path);
        active -= 1;
        resolve(result);
        pump();
      });
    }
  }

  function getAppIcon(params) {
    const path = params?.path;
    if (!isApplicationPath(path) || !applications.has(path)) return Promise.resolve(null);
    const cached = cache.get(path);
    if (cached) {
      cache.delete(path);
      cache.set(path, cached);
      return Promise.resolve(cached.value);
    }
    if (pending.has(path)) return pending.get(path);
    if (pending.size >= limits.maxPending) return Promise.resolve(null);
    let resolve;
    const result = new Promise(done => { resolve = done; });
    pending.set(path, result);
    queue.push({ path, resolve });
    pump();
    return result;
  }

  return Object.freeze({
    rememberApplications,
    getAppIcon,
    getStats: () => ({ entries: cache.size, bytes: cacheBytes, applications: applications.size, pending: pending.size, active }),
  });
}

module.exports = { createAppIconStore, isApplicationPath, extractIcnsPng, bundleIconNames, loadBundleIcon, APP_ICON_LIMITS: LIMITS };
