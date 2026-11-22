'use strict';

const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { execFile } = require('node:child_process');
const { promisify } = require('node:util');

const run = promisify(execFile);
const APPLICATION = /\.app(?:[.-][^/]+)?$/i;
const PACKAGE = /\.(?:app(?:[.-].+)?|bundle|framework|plugin|appex|xpc|kext|prefpane|pkg|qlgenerator|mdimporter|saver|service|action)$/i;
const HELPER = /(?:^|[ ._-])helpers?(?:$|[ ._(\-])/i;
const SKIP = new Set(['node_modules', '.git', '.svn', 'target', 'build', 'dist', 'DerivedData', '.Trash']);
const COMMAND_OPTIONS = { encoding: 'utf8', timeout: 2500, maxBuffer: 4 * 1024 * 1024 };

function normalizedPath(value) {
  return typeof value === 'string' && value.length <= 4096 && path.isAbsolute(value)
    && path.normalize(value) === value && !/[\u0000-\u001f\u007f]/.test(value);
}

function isStandaloneApplicationCandidate(value) {
  if (!normalizedPath(value) || !APPLICATION.test(value)) return false;
  const parts = value.split(path.sep);
  const name = parts.pop().replace(APPLICATION, '');
  return !HELPER.test(name) && !parts.some(part => PACKAGE.test(part) || part.startsWith('.cublet-uninstall-'));
}

// Spotlight is supplemental evidence, never a substitute for bundle validation.
// Search all application bundles: filename-only predicates miss renamed bundles
// whose display name or identifier matches the user's query.
async function findIndexedApplications(query) {
  const escaped = query.replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\*/g, '\\*');
  const match = escaped ? ` && (kMDItemDisplayName == "*${escaped}*"cd || kMDItemFSName == "*${escaped}*"cd || kMDItemCFBundleIdentifier == "*${escaped}*"cd)` : '';
  const { stdout, stderr } = await run('/usr/bin/mdfind', ['-0', `kMDItemContentTypeTree == "com.apple.application-bundle"${match}`], COMMAND_OPTIONS);
  if (stderr.trim()) throw new Error('Spotlight 查询未完整返回');
  return stdout.split('\0').filter(Boolean);
}

async function readPlist(plist) {
  const { stdout, stderr } = await run('/usr/bin/plutil', ['-convert', 'json', '-o', '-', '--', plist], COMMAND_OPTIONS);
  if (stderr.trim()) throw new Error('无法读取应用信息');
  return JSON.parse(stdout);
}

// Check ancestors as well as the leaf. A symlinked directory can otherwise
// expose a nested bundle as a second independent application.
async function inspectPath(target) {
  let current = path.parse(target).root;
  let stat;
  for (const part of target.slice(current.length).split(path.sep).filter(Boolean)) {
    current = path.join(current, part);
    stat = await fs.lstat(current, { bigint: true });
    if (stat.isSymbolicLink()) return null;
    if (current !== target && !stat.isDirectory()) return null;
  }
  return stat || await fs.lstat(current, { bigint: true });
}

function missing(error) { return error?.code === 'ENOENT' || error?.code === 'ENOTDIR'; }
function folded(value) { return value.normalize('NFKD').replace(/\p{M}/gu, '').toLocaleLowerCase(); }

function createApplicationDiscovery({
  home = os.homedir(), applicationsRoot = '/Applications',
  roots = [applicationsRoot, path.join(home, 'Applications')],
  indexedApplications = findIndexedApplications, plist = readPlist,
  now = Date.now, cacheTtlMs = 15_000, maxEntriesPerRoot = 12_000,
  maxDirectoriesPerRoot = 2000, maxDepth = 16, maxCandidates = 4096,
  maxResults = 4096, timeBudgetMs = 5000,
} = {}) {
  const searchRoots = [...new Set(roots.filter(normalizedPath))];
  let baseline;
  let baselinePending;
  let generation = 0;
  let metadataCache = new Map();
  const searches = new Map();

  async function walk() {
    const candidates = new Set();
    const notices = new Set();
    const started = now();
    const states = searchRoots.map(root => ({ queue: [{ target: root, depth: 0 }], index: 0, entries: 0, directories: 0 }));
    const partial = message => notices.add(message);
    // Round-robin breadth-first traversal gives ~/Applications and vendor folders
    // a turn even when the first root contains a large unrelated tree.
    while (states.some(state => state.index < state.queue.length)) {
      if (now() - started > timeBudgetMs) { partial('应用目录发现达到时间上限，已保留找到的应用；可刷新重试。'); break; }
      for (const state of states) {
        const item = state.queue[state.index++];
        if (!item) { state.index = state.queue.length; continue; }
        if (state.directories >= maxDirectoriesPerRoot || state.entries >= maxEntriesPerRoot) {
          partial('部分应用目录达到读取上限，已保留找到的应用。'); state.queue.length = state.index; continue;
        }
        let directory;
        try {
          const stat = await inspectPath(item.target);
          if (!stat?.isDirectory()) continue;
          directory = await fs.opendir(item.target);
          state.directories++;
          for await (const entry of directory) {
            if (++state.entries > maxEntriesPerRoot || now() - started > timeBudgetMs) {
              partial('部分应用目录达到读取上限，已保留找到的应用。'); break;
            }
            if (!entry.isDirectory() || entry.isSymbolicLink()) continue;
            const target = path.join(item.target, entry.name);
            if (isStandaloneApplicationCandidate(target)) {
              if (candidates.size < maxCandidates) candidates.add(target);
              else partial('应用候选数量达到上限，已保留找到的应用。');
            } else if (!PACKAGE.test(entry.name) && !entry.name.startsWith('.') && !SKIP.has(entry.name)) {
              if (item.depth < maxDepth) state.queue.push({ target, depth: item.depth + 1 });
              else partial('部分应用目录超过搜索深度，已保留找到的应用。');
            }
          }
        } catch (error) {
          if (!missing(error)) partial('部分应用目录暂时无法读取，请检查访问权限后刷新。');
        }
      }
    }
    return { candidates, notices };
  }

  async function validate(target) {
    if (!isStandaloneApplicationCandidate(target)) return null;
    const bundle = await inspectPath(target);
    if (!bundle?.isDirectory()) return null; // Finder aliases are regular files.
    const plistPath = path.join(target, 'Contents', 'Info.plist');
    const metadata = await inspectPath(plistPath);
    if (!metadata?.isFile() || metadata.size > 2 * 1024 * 1024) return null;
    const info = await plist(plistPath);
    const executableName = info?.CFBundleExecutable;
    if (info?.CFBundlePackageType !== 'APPL' || typeof executableName !== 'string'
      || !executableName || executableName === '.' || executableName === '..'
      || /[\/\u0000-\u001f\u007f]/.test(executableName)) return null;
    const executable = await inspectPath(path.join(target, 'Contents', 'MacOS', executableName));
    if (!executable?.isFile() || !(executable.mode & 0o111n)) return null;
    const text = value => typeof value === 'string' && value.trim() && value.length < 1024 && !/[\u0000-\u001f\u007f]/.test(value) ? value.trim() : undefined;
    const fallbackName = path.basename(target).replace(APPLICATION, '');
    const name = text(info.CFBundleDisplayName) || text(info.CFBundleName) || fallbackName;
    if (HELPER.test(name) || HELPER.test(executableName)) return null;
    return {
      identity: `${bundle.dev}:${bundle.ino}`,
      application: { path: target, name, bundleId: text(info.CFBundleIdentifier),
        version: text(info.CFBundleShortVersionString) || text(info.CFBundleVersion),
        bundleBytes: 0, supportBytes: 0, sharedData: false,
        bundleBytesKnown: false, measurement: 'unknown' },
    };
  }

  async function collect(query, base, cachedMetadata) {
    const notices = new Set(base.notices);
    const candidates = new Set(base.candidates);
    try {
      const indexed = await indexedApplications(query);
      if (!Array.isArray(indexed)) throw new Error('invalid index result');
      for (const target of indexed) {
        if (!isStandaloneApplicationCandidate(target)) continue;
        if (candidates.size < maxCandidates || candidates.has(target)) candidates.add(target);
        else notices.add('应用候选数量达到上限，已保留找到的应用。');
      }
    } catch {
      notices.add('Spotlight 暂时不可用；已显示应用目录中找到的结果，可刷新重试。');
    }
    const results = new Map();
    const pending = [...candidates];
    let next = 0;
    const started = now();
    await Promise.all(Array.from({ length: Math.min(8, pending.length) }, async () => {
      while (next < pending.length) {
        if (now() - started > timeBudgetMs) { notices.add('部分应用信息尚未读完，已保留找到的应用；可刷新重试。'); break; }
        const target = pending[next++];
        try {
          let validation = cachedMetadata.get(target);
          if (!validation) {
            validation = validate(target);
            if (cachedMetadata.size < maxCandidates) cachedMetadata.set(target, validation);
          }
          const value = await validation;
          if (value) {
            const previous = results.get(value.identity);
            if (!previous || value.application.path.localeCompare(previous.path) < 0) results.set(value.identity, value.application);
          }
        } catch (error) {
          // Stale Spotlight hits and non-application package-shaped folders are
          // expected. Permission failures, however, must not look complete.
          if (error?.code === 'EACCES' || error?.code === 'EPERM') notices.add('部分应用信息无法读取，请检查访问权限后刷新。');
          else if (error?.killed || error?.signal || error?.code === 'ETIMEDOUT'
            || error?.code === 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER') notices.add('部分应用信息读取超时，已保留找到的应用；可刷新重试。');
        }
      }
    }));
    const needle = folded(query);
    const applications = [...results.values()].filter(row => !needle || [row.name, row.path, row.bundleId || ''].some(value => folded(value).includes(needle)))
      .sort((a, b) => a.name.localeCompare(b.name) || a.path.localeCompare(b.path));
    if (applications.length > maxResults) notices.add('应用结果达到显示上限，请输入名称或标识缩小搜索范围。');
    return { applications: applications.slice(0, maxResults), partial: notices.size > 0, notices: [...notices] };
  }

  async function search({ query = '', refresh = false } = {}) {
    if (typeof query !== 'string' || query.length > 512 || /[\u0000-\u001f\u007f]/.test(query)) throw new Error('应用搜索内容无效');
    query = query.trim();
    if (refresh) { generation++; baseline = undefined; baselinePending = undefined; metadataCache = new Map(); searches.clear(); }
    if (baseline && now() - baseline.at >= cacheTtlMs) { baseline = undefined; metadataCache = new Map(); searches.clear(); }
    const cached = searches.get(query);
    if (cached) return structuredClone(await cached);
    const currentGeneration = generation;
    if (!baseline && !baselinePending) {
      baselinePending = walk().then(value => {
        if (currentGeneration === generation) { baseline = { ...value, at: now() }; baselinePending = undefined; }
        return value;
      });
    }
    const base = baseline || baselinePending;
    const cachedMetadata = metadataCache;
    const promise = Promise.resolve(base).then(value => collect(query, value, cachedMetadata));
    searches.set(query, promise);
    // Bound query cache during a long typing session.
    if (searches.size > 32) searches.delete(searches.keys().next().value);
    try { return structuredClone(await promise); }
    catch (error) { if (searches.get(query) === promise) searches.delete(query); throw error; }
  }

  return { search };
}

module.exports = { createApplicationDiscovery, isStandaloneApplicationCandidate, findIndexedApplications };
