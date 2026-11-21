'use strict';

const fs = require('node:fs/promises');
const path = require('node:path');
const { execFile } = require('node:child_process');
const { randomUUID } = require('node:crypto');
const { promisify } = require('node:util');
const { isApplicationPath } = require('./app-icons.cjs');

const run = promisify(execFile);
const PLAN_TTL = 5 * 60_000;
const MAX_PLANS = 16;
const MAX_APPS = 20;
const MAX_INVENTORY = 4096;
const MAX_SEARCH_ENTRIES = 6000;
const MAX_HELPERS = 32;
const MAX_RELATED_PER_APP = 1024;
const MAX_PREVIEW_ENTRIES = MAX_APPS * (MAX_RELATED_PER_APP + 1);
const MAX_PLIST_BYTES = 16n * 1024n * 1024n;
const ID_PATTERN = /^[a-z\d][a-z\d-]*(?:\.[a-z\d][a-z\d-]*)+$/i;
const OWN_ID = 'com.zeeland.codex-storage-manager.electron';
const APP_BACKUP = /\.app[.-].+$/i;
const PACKAGE = /\.(?:app|bundle|framework|plugin|appex|xpc|kext|prefpane|pkg|qlgenerator|mdimporter|saver|service|action)$/i;
const RUNNING = '应用或辅助进程正在使用这些项目，请退出应用后重新预览';
const PROCESS_UNAVAILABLE = '无法确认应用运行状态，请重新预览';
const CHANGED = '项目已发生变化，请重新预览';
const SHARED = '发现使用同一标识的应用副本，关联数据受保护';
const SHARED_UNAVAILABLE = '无法确认是否有其他应用副本，关联数据受保护';
const GROUP_PROTECTED = '应用组容器可能由多个应用使用，成员未核实，数据受保护';
const COMMAND_OPTIONS = {
  encoding: 'utf8',
  timeout: 3000,
  maxBuffer: 16 * 1024 * 1024,
  env: { ...process.env, LC_ALL: 'en_US.UTF-8' },
};

function filePath(value) {
  return typeof value === 'string' && value.length <= 4096 && path.isAbsolute(value)
    && path.normalize(value) === value && !/[\u0000-\u001f\u007f]/.test(value);
}

function packageAncestor(value) {
  return value.split(path.sep).slice(0, -1).some(part => PACKAGE.test(part) || APP_BACKUP.test(part));
}

function applicationPath(value) {
  return filePath(value) && isApplicationPath(value) && !packageAncestor(value);
}

function possibleCopy(value) {
  return filePath(value) && !packageAncestor(value)
    && (isApplicationPath(value) || APP_BACKUP.test(path.basename(value)));
}

function isWithin(parent, child) {
  return child.startsWith(parent + path.sep);
}

function comparisonPath(value) {
  return applicationDiskPath(value).normalize('NFD').toLowerCase();
}

function applicationDiskPath(value) {
  const dataVolume = '/System/Volumes/Data';
  return isWithin(dataVolume, value) ? value.slice(dataVolume.length) : value;
}

function identity(stat) {
  return [stat.dev, stat.ino, stat.birthtimeNs].join(':');
}

function revision(stat) {
  return [identity(stat), stat.mode, stat.size, stat.mtimeNs, stat.ctimeNs].join(':');
}

// Check each ancestor before touching its child. Preserve ancestor identities for execution.
async function inspectPath(target) {
  let current = path.parse(target).root;
  const ancestors = [];
  const parts = target.slice(current.length).split(path.sep);
  for (const [index, part] of parts.entries()) {
    current = path.join(current, part);
    const stat = await fs.lstat(current, { bigint: true });
    if (stat.isSymbolicLink()) throw new Error('符号链接及其子路径受保护');
    if (index === parts.length - 1) {
      if (await fs.realpath(target) !== target) throw new Error(CHANGED);
      return { stat, fingerprint: [...ancestors, revision(stat)].join('/') };
    }
    if (!stat.isDirectory()) throw new Error(CHANGED);
    ancestors.push(identity(stat));
  }
  throw new Error(CHANGED);
}

async function readPlistString(plist, key) {
  const { stdout, stderr } = await run('/usr/bin/plutil', [
    '-extract', key, 'raw', '-expect', 'string', '-n', '-o', '-', '--', plist,
  ], COMMAND_OPTIONS);
  if (stderr) throw new Error('无法读取应用的 Info.plist');
  return stdout;
}

async function readBundleId(plist, plistString) {
  try {
    return await plistString(plist, 'CFBundleIdentifier');
  } catch {
    // Some package-shaped folders have a valid plist with no bundle identifier.
    const { stdout, stderr } = await run('/usr/bin/plutil', ['-convert', 'json', '-o', '-', '--', plist],
      COMMAND_OPTIONS);
    if (stderr) throw new Error(SHARED_UNAVAILABLE);
    const id = JSON.parse(stdout)?.CFBundleIdentifier;
    return typeof id === 'string' ? id : undefined;
  }
}

async function readDeclaredGroupIds(target) {
  try {
    const { stdout } = await run('/usr/bin/codesign', ['--display', '--entitlements', '-', '--', target], COMMAND_OPTIONS);
    const declaration = stdout.match(/<key>com\.apple\.security\.application-groups<\/key>\s*<array>([\s\S]*?)<\/array>/)?.[1];
    if (!declaration) return [];
    return [...new Set([...declaration.matchAll(/<string>([^<]+)<\/string>/g)]
      .map(([, id]) => id).filter(id => id.length < 256 && ID_PATTERN.test(id)))].slice(0, 32);
  } catch {
    // Unsigned fixtures and apps without readable entitlements have no verified hints.
    return [];
  }
}

// Query only validated uninstall targets. A system-wide lsof can block on unrelated
// volumes or exceed its output budget, making every installed app unavailable.
// +D does not follow symlinks or cross mount points (do not add -x).
async function readOpenPaths(targets, runCommand = run) {
  if (!Array.isArray(targets) || !targets.length || targets.some(target => !filePath(target))) {
    throw new Error(PROCESS_UNAVAILABLE);
  }
  const selectors = [];
  const files = [];
  for (const target of new Set(targets)) {
    const stat = await fs.lstat(target);
    if (stat.isSymbolicLink()) throw new Error(PROCESS_UNAVAILABLE);
    if (stat.isDirectory()) selectors.push('+D', target);
    else if (stat.isFile()) files.push(target);
    else throw new Error(PROCESS_UNAVAILABLE);
  }
  const [processes, openFiles] = await Promise.all([
    runCommand('/bin/ps', ['-axww', '-o', 'comm='], COMMAND_OPTIONS),
    runCommand('/usr/sbin/lsof', ['-nP', '-F0fn', ...selectors, '--', ...files], {
      ...COMMAND_OPTIONS, timeout: 6000,
    }).catch(error => {
      // lsof exits 1 when any selected files have no open match, including
      // a +D tree with both open and unopened descendants. Never accept a timeout,
      // truncated buffer or a diagnostic about incomplete process evidence.
      if (error.code === 1 && !error.killed && error.signal == null
        && typeof error.stdout === 'string' && error.stderr === '') return { stdout: error.stdout, stderr: '' };
      throw error;
    }),
  ]);
  if (processes.stderr.trim() || openFiles.stderr.trim()) throw new Error(PROCESS_UNAVAILABLE);
  const executables = processes.stdout.split('\n').map(line => line.trimStart())
    .filter(line => line.startsWith('/'));
  const opened = openFiles.stdout.split('\0')
    .filter(field => field.startsWith('n/')).map(field => field.slice(1));
  if (!executables.length || (openFiles.stdout && !/^p\d+\0/.test(openFiles.stdout))) {
    throw new Error(PROCESS_UNAVAILABLE);
  }
  return [...new Set([...executables, ...opened].map(comparisonPath))];
}

async function measureBytes(target) {
  const { stdout, stderr } = await run('/usr/bin/du', ['-skxP', target], COMMAND_OPTIONS);
  const kilobytes = Number(stdout.match(/^(\d+)\s/)?.[1]);
  if (stderr || !Number.isSafeInteger(kilobytes * 1024)) throw new Error('无法估算项目大小');
  return kilobytes * 1024;
}

async function findIndexedApplications(bundleIds) {
  const query = bundleIds.map(id => `kMDItemCFBundleIdentifier == "${id}"c`).join(' || ');
  const { stdout, stderr } = await run('/usr/bin/mdfind', ['-0', query], COMMAND_OPTIONS);
  if (stderr.trim()) throw new Error(SHARED_UNAVAILABLE);
  return stdout.split('\0').filter(Boolean);
}

async function readDarwinDirectories() {
  const directories = [];
  for (const key of ['DARWIN_USER_CACHE_DIR', 'DARWIN_USER_TEMP_DIR']) {
    const { stdout, stderr } = await run('/usr/bin/getconf', [key], COMMAND_OPTIONS);
    if (stderr.trim()) throw new Error('无法定位当前用户的 Darwin 目录');
    const value = stdout.trim().replace(/\/$/, '');
    if (!path.isAbsolute(value)) throw new Error('Darwin 目录无效');
    const canonical = await fs.realpath(value);
    if (!isWithin('/private/var/folders', canonical)) throw new Error('Darwin 目录边界无效');
    const { stat } = await inspectPath(canonical);
    if (!stat.isDirectory() || stat.uid !== BigInt(process.getuid())) throw new Error('Darwin 目录所有者无效');
    await fs.access(canonical, require('node:fs').constants.W_OK);
    directories.push(canonical);
  }
  return [...new Set(directories)];
}

function inUse(openPaths, target, matchTranslocation = false) {
  const comparable = comparisonPath(target);
  const translocated = `/d/${path.basename(comparable)}/`;
  return openPaths.some(open => open === comparable || isWithin(comparable, open)
    || (matchTranslocation && open.includes('/apptranslocation/') && open.includes(translocated)));
}

function relatedPaths(home, bundleId) {
  const normalizedId = bundleId.toLowerCase();
  if (normalizedId === 'com.apple' || normalizedId.startsWith('com.apple.')) return [];
  const library = path.join(home, 'Library');
  return [
    { kind: 'cache', path: path.join(library, 'Caches', bundleId), directory: true },
    { kind: 'logs', path: path.join(library, 'Logs', bundleId), directory: true },
    { kind: 'logs', path: path.join(library, 'Logs', bundleId + '.log'), directory: false },
    { kind: 'preferences', path: path.join(library, 'Preferences', bundleId + '.plist'), directory: false },
    { kind: 'data', path: path.join(library, 'Application Support', bundleId), directory: true },
    { kind: 'data', path: path.join(library, 'HTTPStorages', bundleId), directory: true },
    { kind: 'data', path: path.join(library, 'Saved Application State', bundleId + '.savedState'), directory: true },
  ];
}

function validateRelated(stat, directory) {
  if (directory ? !stat.isDirectory() : !stat.isFile() || stat.nlink !== 1n) {
    throw new Error('项目类型不符合预期，关联数据受保护');
  }
}

function row(target, kind, appPath) {
  return { id: randomUUID(), path: target, name: path.basename(target), kind, bytes: 0, appPath };
}

function missing(error) {
  return error?.code === 'ENOENT';
}

// Injection points are for the native tests. The renderer supplies only bundle paths and plan IDs.
function createAppUninstaller({
  app,
  shell,
  platform = process.platform,
  home = app.getPath('home'),
  applicationsRoot = '/Applications',
  plistString = readPlistString,
  openPaths = readOpenPaths,
  sizeOf = measureBytes,
  indexedApplications = findIndexedApplications,
  now = Date.now,
  getDisposition,
  dispose,
  sealApplicationItem,
  darwinDirectories = readDarwinDirectories,
  declaredGroupIds = readDeclaredGroupIds,
}) {
  const roots = [...new Set([applicationsRoot, path.join(home, 'Applications')])];
  const currentExecutable = app.getPath('exe');
  const ownBundle = currentExecutable.match(/^(.*?\.app)(?:\/|$)/i)?.[1];
  const plans = new Map();
  const completed = new Map();
  const knownApplications = new Set();
  let inventoryOverflow = false;
  let previewing = false;
  let executing = false;

  const localCandidate = target => filePath(target) && possibleCopy(target)
    && (roots.some(root => isWithin(root, target)) || isWithin(home, target))
    && !isWithin(path.join(home, 'Library'), target)
    && !target.split(path.sep).some(part => part === '.Trash' || part === '.Trashes' || part.startsWith('.cublet-uninstall-'))
    && ![path.join(home, 'Library', 'CloudStorage'), path.join(home, 'Library', 'Mobile Documents')]
      .some(root => target === root || isWithin(root, target));
  const installedPath = target => localCandidate(target)
    && (roots.some(root => isWithin(root, target)) || knownApplications.has(target));

  async function disposition() {
    const value = getDisposition ? await getDisposition() : { mode: 'trash', revision: 'legacy-trash' };
    if (!value || !['trash', 'permanent'].includes(value.mode)
      || typeof value.revision !== 'string' || !value.revision) throw new Error('无法读取处理方式，请检查设置后重新预览');
    return { mode: value.mode, revision: value.revision };
  }
  const ownApplication = (target, bundleId) => (bundleId?.toLowerCase() === OWN_ID && (!getDisposition || !ownBundle))
    || comparisonPath(target) === comparisonPath(currentExecutable)
    || isWithin(comparisonPath(target), comparisonPath(currentExecutable))
    || (ownBundle && comparisonPath(target) === comparisonPath(ownBundle));

  async function verifiedRunningOwnBundle() {
    if (!getDisposition || !ownBundle) return false;
    try {
      const { plist } = await bundleMetadata(ownBundle);
      await inspectPath(currentExecutable);
      return (await plistString(plist, 'CFBundleIdentifier')).toLowerCase() === OWN_ID;
    } catch { return false; }
  }

  function listApplications(applications, viewPath) {
    const owners = new Map();
    const installed = new Map();
    const scope = viewPath && applicationDiskPath(viewPath);
    for (const entry of applications) {
      const target = applicationDiskPath(entry.path);
      for (const associated of entry.associatedPaths) {
        const dataPath = applicationDiskPath(associated);
        if (!owners.has(dataPath)) owners.set(dataPath, target);
        else if (owners.get(dataPath) !== target) owners.set(dataPath, null);
      }
      if (!installedPath(target) || ownApplication(target, entry.bundleId)) continue;
      if (scope && scope !== target && !isWithin(scope, target)) continue;
      if (!installed.has(target)) installed.set(target, { ...entry, path: target });
    }
    return [...installed.values()].map(({ associatedPaths, ...entry }) => ({
      ...entry,
      sharedData: associatedPaths.some(associated => owners.get(applicationDiskPath(associated)) === null),
    }));
  }

  function rememberApplications(applications, copiesTruncated = false) {
    if (copiesTruncated) inventoryOverflow = true;
    if (!Array.isArray(applications)) return;
    for (const entry of applications) {
      if (!possibleCopy(entry?.path)) continue;
      if (knownApplications.size >= MAX_INVENTORY && !knownApplications.has(entry.path)) {
        inventoryOverflow = true;
        continue;
      }
      knownApplications.add(entry.path);
    }
  }

  async function bundleMetadata(target) {
    const bundle = await inspectPath(target);
    if (!bundle.stat.isDirectory()) throw new Error('应用包必须是普通目录');
    const plist = path.join(target, 'Contents', 'Info.plist');
    const metadata = await inspectPath(plist);
    if (!metadata.stat.isFile() || metadata.stat.size > MAX_PLIST_BYTES) {
      throw new Error('应用的 Info.plist 无效');
    }
    return { bundle, plist, metadata };
  }

  async function validateApp(target) {
    if (!installedPath(target)) {
      throw new Error('仅支持 Applications 目录或已发现的本地独立 macOS 应用');
    }
    if (ownApplication(target)) {
      throw new Error('磁盘管家自身受保护');
    }
    const { bundle, plist, metadata } = await bundleMetadata(target);
    if (typeof process.getuid === 'function' && bundle.stat.uid !== BigInt(process.getuid())) {
      throw new Error('其他用户拥有的应用受保护');
    }
    let bundleId, type, executableName;
    try {
      [bundleId, type, executableName] = await Promise.all([
        plistString(plist, 'CFBundleIdentifier'), plistString(plist, 'CFBundlePackageType'),
        plistString(plist, 'CFBundleExecutable'),
      ]);
    } catch {
      throw new Error('无法验证应用的 Info.plist');
    }
    if (type !== 'APPL' || typeof bundleId !== 'string' || bundleId.length >= 256
      || !ID_PATTERN.test(bundleId) || typeof executableName !== 'string' || !executableName
      || executableName === '.' || executableName === '..' || /[\/\u0000-\u001f\u007f]/.test(executableName)) {
      throw new Error('无法验证独立应用包及其可执行文件');
    }
    if (ownApplication(target, bundleId)
      || (bundleId.toLowerCase() === OWN_ID && !await verifiedRunningOwnBundle())) throw new Error('磁盘管家自身受保护');
    const executablePath = path.join(target, 'Contents', 'MacOS', executableName);
    const executable = await inspectPath(executablePath);
    if (!executable.stat.isFile() || !(executable.stat.mode & 0o111n)) {
      throw new Error('应用缺少有效的可执行文件');
    }
    return {
      bundleId, identity: identity(bundle.stat),
      stamps: [[target, bundle.fingerprint], [plist, metadata.fingerprint], [executablePath, executable.fingerprint]],
    };
  }

  async function findApplicationCandidates() {
    // Ordinary files inside Applications cannot contain bundles. Budget directory
    // discovery separately from metadata verification and preserve every hit when
    // one root is partial. A partial root never proves absence of another copy.
    const candidates = new Set(knownApplications);
    const coverage = [];
    for (const root of roots) {
      let count = 0;
      let partial = false;
      async function scan(directory, depth) {
        if (depth > 16 || ++count > MAX_SEARCH_ENTRIES) { partial = true; return; }
        const { stat } = await inspectPath(directory);
        if (!stat.isDirectory()) throw new Error(SHARED_UNAVAILABLE);
        const entries = await fs.opendir(directory);
        for await (const entry of entries) {
          if (!entry.isDirectory() && !entry.isSymbolicLink()) continue;
          const entryPath = path.join(directory, entry.name);
          if (possibleCopy(entryPath)) candidates.add(entryPath);
          else if (entry.isDirectory() && !PACKAGE.test(entry.name) && !APP_BACKUP.test(entry.name)) {
            if (count >= MAX_SEARCH_ENTRIES) { partial = true; continue; }
            try { await scan(entryPath, depth + 1); } catch { partial = true; }
          }
          if (candidates.size >= MAX_SEARCH_ENTRIES) { partial = true; break; }
        }
      }
      try { await scan(root, 0); } catch (error) { if (!missing(error)) partial = true; }
      coverage.push({ root, partial, directories: count });
    }
    return { candidates, coverage };
  }

  async function sharingEvidence(groups, knownIdentifiers = new Map(), verifiedEvidence) {
    const requestedIds = new Set(groups.flatMap(group => [group.app.bundleId,
      ...group.entries.map(entry => entry.item.ownerId).filter(Boolean)]).map(id => id.toLowerCase()));
    const copies = new Map([...requestedIds].map(id => [id, new Map(verifiedEvidence?.copies.get(id))]));
    // Only the selected identifiers' live lookup can make absence unavailable.
    // Broad discovery still finds real copies, but its unrelated gaps are diagnostics.
    let incomplete = false;
    const coverage = verifiedEvidence ? [] : [{ source: 'past-inventory', partial: inventoryOverflow }];
    const [broad, indexed] = await Promise.allSettled([
      verifiedEvidence ? { candidates: [], coverage: [] } : findApplicationCandidates(), indexedApplications([...requestedIds]),
    ]);
    const candidates = new Map();
    for (const [source, indexedSource] of [[indexed, true], [broad, false]]) {
      if (source.status === 'rejected' || (indexedSource && !Array.isArray(source.value))) {
        if (indexedSource) incomplete = true;
        coverage.push({ source: indexedSource ? 'live-index' : 'application-roots', partial: true });
        continue;
      }
      const values = indexedSource ? source.value : source.value.candidates;
      if (!indexedSource) {
        coverage.push(...source.value.coverage.map(value => ({ source: 'application-root', ...value })));
      } else coverage.push({ source: 'live-index', partial: false });
      let capped = false;
      for (const candidate of values) {
        if (!filePath(candidate)) {
          if (indexedSource) incomplete = true;
          continue;
        }
        if (!possibleCopy(candidate)) continue;
        if (candidates.size >= MAX_SEARCH_ENTRIES && !candidates.has(candidate)) {
          capped = true;
          if (indexedSource) incomplete = true;
          continue;
        }
        // Indexed matches are inserted first, ahead of capped broad candidates.
        candidates.set(candidate, indexedSource || candidates.get(candidate) || false);
      }
      if (capped) coverage.push({ source: indexedSource ? 'live-index-budget' : 'broad-candidate-budget', partial: true });
    }
    let metadataPartial = false;
    let indexedMetadataPartial = false;
    const pending = candidates.entries();
    await Promise.all(Array.from({ length: 8 }, async () => {
      for (const [candidate, indexedSource] of pending) {
        let canonical;
        try {
          canonical = await fs.realpath(candidate);
          if (!possibleCopy(canonical)) continue;
          const { bundle, plist, metadata } = await bundleMetadata(canonical);
          let cached = knownIdentifiers.get(plist);
          if (cached?.fingerprint !== metadata.fingerprint) {
            cached = { fingerprint: metadata.fingerprint, id: await readBundleId(plist, plistString) };
            if (knownIdentifiers.size < MAX_SEARCH_ENTRIES) knownIdentifiers.set(plist, cached);
          }
          const id = cached.id;
          if (id !== undefined && (typeof id !== 'string' || id.length >= 256 || !ID_PATTERN.test(id))) {
            throw new Error(SHARED_UNAVAILABLE);
          }
          if (!id) {
            if (indexedSource) throw new Error(SHARED_UNAVAILABLE);
            continue;
          }
          // A valid different identifier disproves a stale indexed match.
          const matchingCopies = copies.get(id.toLowerCase());
          if (!matchingCopies) continue;
          const physicalIdentity = identity(bundle.stat);
          const previous = matchingCopies.get(physicalIdentity);
          if (!previous || canonical < previous) matchingCopies.set(physicalIdentity, canonical);
        } catch (error) {
          // A missing indexed bundle is a harmless stale hit. A present bundle
          // whose Info.plist is missing or unreadable still lacks matching proof.
          if (!canonical && missing(error)) continue;
          metadataPartial = true;
          if (indexedSource) { incomplete = true; indexedMetadataPartial = true; }
        }
      }
    }));
    coverage.push({ source: 'candidate-metadata', partial: metadataPartial, candidates: candidates.size });
    if (indexedMetadataPartial) coverage.push({ source: 'live-index-metadata', partial: true });
    const evidence = { incomplete, copies, coverage };
    if (incomplete && groups.length > 1) {
      // A batch failure cannot identify the affected app. Retry just the exact
      // identifiers per group, reusing all verified copies without rescanning roots.
      evidence.scoped = new Map();
      const pendingGroups = groups.values();
      await Promise.all(Array.from({ length: Math.min(4, groups.length) }, async () => {
        for (const group of pendingGroups) {
          const scoped = await sharingEvidence([group], knownIdentifiers, evidence);
          evidence.scoped.set(group.item.path, scoped);
          coverage.push(...scoped.coverage.map(row => ({ ...row, appPath: group.item.path })));
        }
      }));
    }
    return evidence;
  }

  function sharingReason(group, evidence, ownerId = group.app.bundleId) {
    const { incomplete, copies } = evidence.scoped?.get(group.item.path) || evidence;
    const otherCopies = copies.get(ownerId.toLowerCase());
    if ([...(otherCopies?.keys() || [])].some(copy => copy !== group.app.identity)) return SHARED;
    return incomplete ? SHARED_UNAVAILABLE : undefined;
  }

  function sharedCopyPaths(group, evidence) {
    const { copies } = evidence.scoped?.get(group.item.path) || evidence;
    const matches = new Map();
    for (const ownerId of new Set([group.app.bundleId, ...group.entries.map(entry => entry.item.ownerId).filter(Boolean)])) {
      for (const [physicalIdentity, canonical] of copies.get(ownerId.toLowerCase()) || []) {
        if (physicalIdentity === group.app.identity) continue;
        const previous = matches.get(physicalIdentity);
        if (!previous || canonical < previous) matches.set(physicalIdentity, canonical);
      }
    }
    return [...matches.values()].sort();
  }

  async function addSize(item, warnings) {
    try {
      const bytes = await sizeOf(item.path);
      if (Number.isSafeInteger(bytes) && bytes >= 0) {
        item.bytes = bytes;
        return;
      }
    } catch { /* Size estimates are independent of target validation. */ }
    warnings.add('部分项目无法估算大小，列表中的大小可能偏低');
  }

  async function inspectUsage(targets) {
    try {
      const paths = await openPaths(targets);
      if (Array.isArray(paths)) return { paths: paths.map(comparisonPath) };
    } catch { /* Fail closed when native process evidence is unavailable. */ }
    return { blockedReason: PROCESS_UNAVAILABLE };
  }

  async function relatedCandidates(target, validated) {
    const id = validated.bundleId;
    const candidates = relatedPaths(home, id).map(value => ({
      ...value, evidence: 'exact-bundle-id', strength: 'strong', ownerId: id,
    }));
    const warnings = [];
    if (id.toLowerCase().startsWith('com.apple.')) return { candidates, warnings };
    const add = (candidate, evidence, ownerId = id, strength = 'strong') => {
      if (candidates.length >= MAX_RELATED_PER_APP) {
        if (!warnings.includes('关联项目候选达到检查上限，部分位置未列出')) warnings.push('关联项目候选达到检查上限，部分位置未列出');
        return;
      }
      candidates.push({ ...candidate, evidence, ownerId, strength });
    };
    // Only app-owned helper metadata can expand the identifier set. Never infer
    // helper ownership by fuzzy vendor or product-name matches in Library.
    const helperIds = new Set();
    const helperPaths = new Set();
    const helperStamps = [];
    for (const directory of [path.join(target, 'Contents', 'Frameworks'), path.join(target, 'Contents', 'Helpers')]) {
      try {
        const { stat } = await inspectPath(directory);
        if (!stat.isDirectory()) continue;
        let count = 0;
        for await (const entry of await fs.opendir(directory)) {
          if (++count > 256) { warnings.push('辅助应用元数据检查不完整'); break; }
          if (!entry.isDirectory() || !/\.app$/i.test(entry.name)) continue;
          const helper = path.join(directory, entry.name);
          try {
            const metadata = await bundleMetadata(helper);
            const helperId = await readBundleId(metadata.plist, plistString);
            if (!ID_PATTERN.test(helperId || '') || !helperId.toLowerCase().startsWith(id.toLowerCase() + '.')) continue;
            if (helperPaths.size >= MAX_HELPERS) {
              if (!warnings.includes('辅助应用标识达到检查上限，部分关联位置未列出')) warnings.push('辅助应用标识达到检查上限，部分关联位置未列出');
              continue;
            }
            helperPaths.add(helper);
            helperIds.add(helperId);
            helperStamps.push([helper, metadata.bundle.fingerprint], [metadata.plist, metadata.metadata.fingerprint]);
          } catch { warnings.push('部分辅助应用标识无法核实'); }
        }
      } catch (error) { if (!missing(error)) warnings.push('辅助应用元数据不可访问'); }
    }
    for (const helperId of helperIds) {
      for (const candidate of relatedPaths(home, helperId)) add(candidate, 'embedded-helper-bundle-id', helperId);
    }
    // ShipIt is offered only when the selected bundle actually ships Squirrel's
    // updater executable; the conventional suffix alone is not ownership proof.
    const squirrel = path.join(target, 'Contents', 'Frameworks', 'Squirrel.framework');
    const shipitCandidates = [path.join(squirrel, 'Resources', 'ShipIt')];
    try {
      const versions = path.join(squirrel, 'Versions');
      if ((await inspectPath(versions)).stat.isDirectory()) {
        let count = 0;
        for await (const entry of await fs.opendir(versions)) {
          if (++count > 32) { warnings.push('更新器版本检查不完整'); break; }
          if (entry.isDirectory()) shipitCandidates.push(path.join(versions, entry.name, 'Resources', 'ShipIt'));
        }
      }
    } catch { /* Frameworks without version directories are supported. */ }
    for (const shipit of shipitCandidates) {
      try {
        const value = await inspectPath(shipit);
        if (value.stat.isFile() && (value.stat.mode & 0o111n)) {
          helperStamps.push([shipit, value.fingerprint]);
          add({ kind: 'cache', path: path.join(home, 'Library', 'Caches', id + '.ShipIt'), directory: true }, 'embedded-squirrel-updater');
          break;
        }
      } catch { /* Never follow Resources/Current aliases; inspect actual versions. */ }
    }
    try {
      const directory = path.join(home, 'Library', 'Preferences', 'ByHost');
      const { stat } = await inspectPath(directory);
      if (stat.isDirectory()) {
        let count = 0;
        for await (const entry of await fs.opendir(directory)) {
          if (++count > MAX_SEARCH_ENTRIES) { warnings.push('ByHost 偏好检查不完整'); break; }
          for (const owner of [id, ...helperIds]) {
            const suffix = entry.name.slice(owner.length);
            if (entry.name.startsWith(owner + '.') && /^\.[a-f\d]{8}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{12}\.plist$/i.test(suffix)) {
              add({ kind: 'preferences', path: path.join(directory, entry.name), directory: false }, 'exact-byhost-owner', owner);
            }
          }
        }
      }
    } catch (error) { if (!missing(error)) warnings.push('ByHost 偏好不可访问'); }
    for (const groupId of (await declaredGroupIds(target)).slice(0, 32)) {
      if (typeof groupId !== 'string' || groupId.length >= 256 || !ID_PATTERN.test(groupId)) continue;
      add({ kind: 'data', path: path.join(home, 'Library', 'Group Containers', groupId), directory: true,
        protectedReason: GROUP_PROTECTED }, 'declared-group-container', id, 'weak');
    }
    try {
      for (const directory of await darwinDirectories()) {
        const { stat } = await inspectPath(directory);
        if (!stat.isDirectory() || (typeof process.getuid === 'function' && stat.uid !== BigInt(process.getuid()))) continue;
        await fs.access(directory, require('node:fs').constants.W_OK);
        for (const owner of [id, ...helperIds]) add({ kind: 'cache', path: path.join(directory, owner), directory: true }, 'darwin-current-user-exact-id', owner);
      }
    } catch { warnings.push('当前用户的 Darwin 缓存或临时目录未能完整检查'); }
    const container = path.join(home, 'Library', 'Containers', id);
    try {
      const metadataPath = path.join(container, '.com.apple.containermanagerd.metadata.plist');
      const metadata = await inspectPath(metadataPath);
      if (metadata.stat.isFile() && metadata.stat.size <= MAX_PLIST_BYTES) {
        const owner = await plistString(metadataPath, 'MCMMetadataIdentifier');
        if (owner === id) add({ kind: 'data', path: container, directory: true, ownershipStamps: [[metadataPath, metadata.fingerprint]] }, 'container-metadata-exact-id');
        else add({ kind: 'data', path: container, directory: true }, 'container-owner-unverified', id, 'weak');
      }
    } catch (error) {
      if (!missing(error)) warnings.push('沙盒容器归属无法核实');
      // A present container without usable owner metadata remains visible.
      try { await fs.lstat(container); add({ kind: 'data', path: container, directory: true }, 'container-owner-unverified', id, 'weak'); } catch { /* Absent. */ }
    }
    const names = new Set([path.basename(target).replace(/\.app(?:[.-].+)?$/i, '')]);
    for (const key of ['CFBundleName', 'CFBundleDisplayName']) {
      try {
        const name = await plistString(path.join(target, 'Contents', 'Info.plist'), key);
        if (typeof name === 'string' && name.length <= 255 && name && !/[\/\u0000-\u001f\u007f]/.test(name)) names.add(name);
      } catch { /* Optional display names are weak hints, never authorization. */ }
    }
    for (const appName of names) {
      if (appName && appName !== '.' && appName !== '..' && appName !== id) {
        add({ kind: 'data', path: path.join(home, 'Library', 'Application Support', appName), directory: true }, 'application-name-only', id, 'weak');
      }
    }
    validated.stamps.push(...helperStamps);
    return { candidates, warnings };
  }

  async function prepareGroup(target, identities) {
    const item = row(target, 'app', target);
    const entries = [{ item }];
    let validated;
    try {
      validated = await validateApp(target);
    } catch (error) {
      item.blockedReason = missing(error) ? '应用已不存在，请重新扫描' : error.message;
      return { item, entries };
    }
    if (identities.has(validated.identity)) return null;
    identities.add(validated.identity);
    item.evidence = 'verified-independent-bundle';
    item.evidenceStrength = 'strong';
    item.ownerId = validated.bundleId;
    const discovery = await relatedCandidates(target, validated);
    for (const related of discovery.candidates) {
      if (entries.some(entry => entry.item.path === related.path)) continue;
      const candidate = { ...row(related.path, related.kind, target), evidence: related.evidence,
        evidenceStrength: related.strength, ownerId: related.ownerId, shared: false, protected: false };
      if (validated.bundleId.toLowerCase() === OWN_ID) candidate.blockedReason = '磁盘管家当前使用的关联数据受保护';
      if (related.strength === 'weak') candidate.blockedReason = related.protectedReason || '仅发现名称或未核实的归属线索，关联数据受保护';
      try {
        const { stat, fingerprint } = await inspectPath(related.path);
        validateRelated(stat, related.directory);
        if (typeof process.getuid === 'function' && stat.uid !== BigInt(process.getuid())) {
          candidate.blockedReason = '其他用户拥有的关联数据受保护';
        }
        entries.push({ item: candidate, directory: related.directory, stamps: [[related.path, fingerprint], ...(related.ownershipStamps || [])] });
      } catch (error) {
        if (missing(error)) continue;
        candidate.blockedReason = error.message;
        entries.push({ item: candidate });
      }
    }
    entries[0].stamps = validated.stamps;
    return { app: validated, item, entries, warnings: discovery.warnings };
  }

  async function prepareRecovery(target, receipt, identities) {
    const previous = receipt.groups.find(group => group.item.path === target);
    if (!previous) throw new Error('恢复范围不属于之前的操作，请重新扫描');
    const failedPaths = new Set(receipt.result.failures.map(item => item.path));
    const failed = previous.entries.filter(entry => receipt.selected.has(entry.item.id) && failedPaths.has(entry.item.path));
    if (failed.length === 0) return null;
    if (!receipt.removedApps.has(target)) {
      if (!failed.some(entry => entry.item.kind === 'app')) throw new Error('该应用没有可恢复的已选失败项目');
      if (!previous.entries[0].stamps) throw new Error('之前未能验证该应用，请重新扫描');
      await verify(previous.entries[0].stamps);
      const fresh = await prepareGroup(target, identities);
      const allowed = new Set(failed.map(entry => entry.item.path));
      fresh.entries = fresh.entries.filter(entry => allowed.has(entry.item.path));
      return fresh;
    }
    await inspectPath(path.dirname(target));
    await confirmAppAbsent(target);
    const eligible = failed.filter(entry => entry.item.kind !== 'app' && entry.stamps
      && entry.item.evidenceStrength === 'strong' && entry.item.ownerId
      && previous.app.bundleId.toLowerCase() !== OWN_ID);
    if (!eligible.length) throw new Error('没有可恢复的已选关联项目，请重新扫描');
    const item = { ...previous.item, id: randomUUID(), bytes: 0, alreadyRemoved: true, protected: false };
    delete item.blockedReason;
    const entries = [{ item }];
    for (const original of eligible) {
      const candidate = { ...original.item, id: randomUUID(), protected: false, shared: false };
      delete candidate.blockedReason;
      delete candidate.copyProtection;
      const entry = { item: candidate, directory: original.directory, stamps: original.stamps };
      try {
        const expected = original.stamps.find(([file]) => file === candidate.path)?.[1];
        if (!expected) throw new Error(CHANGED);
        const current = await inspectPath(candidate.path);
        // Recovery may remeasure changed contents or repaired permissions, but
        // never approve a replacement object or changed ancestor ownership.
        const prefix = expected.slice(0, expected.lastIndexOf('/') + 1);
        if (!current.fingerprint.startsWith(prefix)
          || identity(current.stat) !== expected.slice(expected.lastIndexOf('/') + 1).split(':').slice(0, 3).join(':')) throw new Error(CHANGED);
        entry.directory ??= current.stat.isDirectory();
        validateRelated(current.stat, entry.directory);
        if (typeof process.getuid === 'function' && current.stat.uid !== BigInt(process.getuid())) throw new Error('其他用户拥有的关联数据受保护');
        const ownership = original.stamps.filter(([file]) => file !== candidate.path);
        await verify(ownership);
        if (candidate.evidence === 'container-metadata-exact-id') {
          const metadata = path.join(candidate.path, '.com.apple.containermanagerd.metadata.plist');
          if (await plistString(metadata, 'MCMMetadataIdentifier') !== candidate.ownerId) throw new Error(CHANGED);
        }
        await fs.access(path.dirname(candidate.path), require('node:fs').constants.W_OK);
        entry.stamps = [[candidate.path, current.fingerprint], ...ownership];
      } catch (error) { candidate.blockedReason = missing(error) ? '关联项目已不存在，请重新扫描' : error.message; }
      entries.push(entry);
    }
    return { app: previous.app, item, entries, alreadyRemoved: true };
  }

  async function preview(paths, retryPlanId) {
    if (platform !== 'darwin') throw new Error('应用卸载仅支持 macOS');
    if (!Array.isArray(paths) || !paths.length || paths.length > MAX_APPS || paths.some(p => !filePath(p))) {
      throw new Error(`请选择 1 至 ${MAX_APPS} 个有效的应用路径`);
    }
    if (previewing || executing) throw new Error('正在处理另一项应用卸载，请稍后重试');
    previewing = true;
    try {
      const policy = await disposition();
      let retryReceipt;
      if (retryPlanId !== undefined) {
        retryReceipt = completed.get(retryPlanId);
        if (!retryReceipt || retryReceipt.expires <= now() || !retryReceipt.groups || !retryReceipt.removedApps) {
          throw new Error('恢复记录已失效，请重新扫描');
        }
      }
      const warnings = new Set();
      const groups = [];
      const identities = new Set();
      for (const target of new Set(paths)) {
        const group = retryReceipt ? await prepareRecovery(target, retryReceipt, identities) : await prepareGroup(target, identities);
        if (group) { groups.push(group); for (const warning of group.warnings || []) warnings.add(warning); }
      }
      const withData = groups.filter(group => group.entries.some(entry => entry.item.kind !== 'app' && entry.stamps));
      const valid = groups.filter(group => group.app);
      const usageTargets = valid.flatMap(group => group.entries.filter(entry => entry.stamps && !entry.item.blockedReason).map(entry => entry.item.path));
      const [usage, sharing] = await Promise.all([
        usageTargets.length ? inspectUsage(usageTargets) : { paths: [] },
        withData.length ? sharingEvidence(withData) : undefined,
      ]);
      for (const group of valid) {
        group.item.blockedReason = group.item.blockedReason || usage.blockedReason
          || (!group.alreadyRemoved && inUse(usage.paths, group.item.path, true) ? RUNNING : undefined);
        const relatedBlock = group.item.blockedReason || (withData.includes(group) ? sharingReason(group, sharing) : undefined);
        for (const entry of group.entries.slice(1)) {
          entry.item.blockedReason ||= group.item.blockedReason || (inUse(usage.paths, entry.item.path) ? RUNNING : undefined)
            || relatedBlock || (sharing && sharingReason(group, sharing, entry.item.ownerId));
          if (entry.item.blockedReason === SHARED) entry.item.copyProtection = 'shared';
          else if (entry.item.blockedReason === SHARED_UNAVAILABLE) entry.item.copyProtection = 'unavailable';
          entry.item.shared = entry.item.blockedReason === SHARED || entry.item.evidence === 'declared-group-container';
          entry.item.protected = Boolean(entry.item.blockedReason);
        }
      }
      // A shared physical path is displayed and measured once. It remains tied
      // to its first owner row and records the other selected owners explicitly.
      const seenPaths = new Map();
      for (const group of groups) {
        group.entries = group.entries.filter(entry => {
          const existing = seenPaths.get(entry.item.path);
          if (!existing) { seenPaths.set(entry.item.path, entry); return true; }
          existing.item.sharedWithAppPaths ||= [];
          existing.item.sharedWithAppPaths.push(group.item.path);
          existing.item.shared = true;
          return false;
        });
      }
      const entries = groups.flatMap(group => group.entries);
      if (entries.length > MAX_PREVIEW_ENTRIES) throw new Error('预览项目过多，请缩小应用选择');
      for (const entry of entries) entry.item.protected = Boolean(entry.item.blockedReason);
      const pending = entries.values();
      await Promise.all(Array.from({ length: 4 }, async () => {
        for (const entry of pending) {
          if (entry.stamps) await addSize(entry.item, warnings);
          if (policy.mode === 'permanent' && sealApplicationItem && entry.stamps && !entry.item.blockedReason) {
            try {
              await verify(entry.stamps);
              const seal = await sealApplicationItem(entry.stamps.map(([path, fingerprint]) => ({ path, fingerprint })));
              if (typeof seal !== 'string' || !seal) throw new Error('无效的目录边界');
              await verify(entry.stamps);
              entry.seal = seal;
            } catch {
              entry.item.blockedReason = '无法核实永久删除的目录边界，请重新预览';
              entry.item.protected = true;
            }
          }
        }
      }));
      for (const group of groups) {
        if (!group.item.blockedReason) continue;
        for (const entry of group.entries.slice(1)) {
          entry.item.blockedReason ||= group.item.blockedReason;
          entry.item.protected = true;
        }
      }
      const planId = randomUUID();
      for (const [id, plan] of plans) {
        if (plan.expires <= now()) plans.delete(id);
      }
      while (plans.size >= MAX_PLANS) plans.delete(plans.keys().next().value);
      plans.set(planId, { groups, entries, ...policy, expires: now() + PLAN_TTL });
      const items = entries.map(({ item }) => ({ ...item }));
      const sharedCopies = sharing ? withData.map(group => ({ appPath: group.item.path, paths: sharedCopyPaths(group, sharing) }))
        .filter(group => group.paths.length).sort((a, b) => a.appPath < b.appPath ? -1 : a.appPath > b.appPath ? 1 : 0) : [];
      return { planId, mode: policy.mode, items, warnings: [...warnings], coverage: sharing?.coverage || [],
        ...(sharedCopies.length ? { sharedCopies } : {}) };
    } finally {
      previewing = false;
    }
  }

  async function verify(stamps) {
    for (const [target, expected] of stamps) {
      try {
        if ((await inspectPath(target)).fingerprint === expected) continue;
      } catch { /* Present a consistent stale-plan message. */ }
      throw new Error(CHANGED);
    }
  }

  async function checkUnused(target, matchTranslocation) {
    const usage = await inspectUsage([target]);
    if (usage.blockedReason) throw new Error(usage.blockedReason);
    if (inUse(usage.paths, target, matchTranslocation)) throw new Error(RUNNING);
  }

  async function confirmAppAbsent(target) {
    try {
      await fs.lstat(target);
    } catch (error) {
      if (missing(error)) return;
      throw error;
    }
    throw new Error('应用路径仍然存在，请重新预览');
  }

  async function execute(planId, selectedIds, mode) {
    if (platform !== 'darwin') throw new Error('应用卸载仅支持 macOS');
    if (previewing || executing) throw new Error('正在处理另一项应用卸载，请稍后重试');
    const receipt = completed.get(planId);
    if (receipt && receipt.expires > now()) {
      if (mode !== receipt.result.mode || !Array.isArray(selectedIds)
        || selectedIds.length !== receipt.selected.size || new Set(selectedIds).size !== selectedIds.length
        || selectedIds.some(id => !receipt.selected.has(id))) {
        throw new Error('选择与已完成的操作不一致，请重新预览');
      }
      return structuredClone(receipt.result);
    }
    completed.delete(planId);
    const plan = plans.get(planId);
    if (!plan || plan.expires <= now()) {
      plans.delete(planId);
      throw new Error('卸载预览已失效，请重新预览');
    }
    if (!Array.isArray(selectedIds) || !selectedIds.length || selectedIds.length > plan.entries.length) {
      throw new Error('请选择预览中的项目');
    }
    const selected = new Set(selectedIds);
    if (selected.size !== selectedIds.length || selectedIds.some(id => !plan.entries.some(entry => entry.item.id === id))) {
      throw new Error('请选择预览中的项目');
    }
    executing = true;
    let policy;
    try { policy = await disposition(); } catch (error) { executing = false; throw error; }
    if (policy.mode !== plan.mode || policy.revision !== plan.revision || (getDisposition && mode !== plan.mode)) {
      plans.delete(planId);
      executing = false;
      throw new Error('处理方式已变更或确认方式不匹配，请重新预览');
    }
    plans.delete(planId);
    const movedPaths = [];
    const movedAppPaths = [];
    const failures = [];
    const confirmedApps = new Set();
    const knownIdentifiers = new Map();
    async function move(entry, beforeMove) {
      const { item, stamps } = entry;
      try {
        if (item.blockedReason) throw new Error(item.blockedReason);
        if (item.path.split(path.sep).some(part => part.startsWith('.cublet-uninstall-'))) throw new Error('中断操作的暂存项目受保护，请先检查恢复状态');
        // Reject a replaced target before querying its recursive process usage.
        // The second check still catches changes during those native queries.
        await verify(stamps);
        await beforeMove();
        await verify(stamps);
        if (dispose) {
          await dispose(item, plan.mode, {
            seal: entry.seal,
            stamps: stamps.map(([path, fingerprint]) => ({ path, fingerprint })),
            verify: () => verify(stamps),
          });
        } else {
          if (plan.mode !== 'trash') throw new Error('永久删除服务不可用，请重新预览');
          await shell.trashItem(item.path);
        }
        await confirmAppAbsent(item.path);
        movedPaths.push(item.path);
        if (item.kind === 'app') {
          movedAppPaths.push(item.path);
          confirmedApps.add(item.path);
        }
      } catch (error) {
        failures.push({ path: item.path, message: error.message || (plan.mode === 'trash' ? '无法移到废纸篓' : '无法永久删除') });
      }
    }

    try {
      for (const group of plan.groups) {
        if (group.alreadyRemoved) {
          try { await inspectPath(path.dirname(group.item.path)); await confirmAppAbsent(group.item.path); confirmedApps.add(group.item.path); } catch { /* Related rows recheck absence and retain their data. */ }
          continue;
        }
        if (selected.has(group.item.id)) {
          await move(group.entries[0], () => checkUnused(group.item.path, true));
        }
      }
      for (const group of plan.groups) {
        for (const entry of group.entries.slice(1)) {
          if (!selected.has(entry.item.id)) continue;
          await move(entry, async () => {
            if (!confirmedApps.has(group.item.path)) {
              throw new Error(plan.mode === 'trash' ? '请先将对应应用移到废纸篓' : '请先永久删除对应应用');
            }
            await checkUnused(entry.item.path);
            const evidence = await sharingEvidence([group], knownIdentifiers);
            const blocked = sharingReason(group, evidence) || sharingReason(group, evidence, entry.item.ownerId);
            if (blocked) throw new Error(blocked);
            await confirmAppAbsent(group.item.path);
          });
        }
      }
      const recoverableRelated = plan.groups.some(group => confirmedApps.has(group.item.path)
        && group.entries.some(entry => entry.item.kind !== 'app' && selected.has(entry.item.id)
          && entry.stamps && entry.item.evidenceStrength === 'strong' && failures.some(failure => failure.path === entry.item.path)));
      const result = { mode: plan.mode, movedPaths, movedAppPaths, failures, ...(recoverableRelated ? { recoverableRelated: true } : {}) };
      if (getDisposition) {
        while (completed.size >= MAX_PLANS) completed.delete(completed.keys().next().value);
        completed.set(planId, { selected, groups: plan.groups, removedApps: confirmedApps, expires: now() + PLAN_TTL, result: structuredClone(result) });
      }
      return result;
    } finally {
      executing = false;
    }
  }

  return { listApplications, preview, execute, rememberApplications };
}

module.exports = { applicationDiskPath, createAppUninstaller, readOpenPaths, measureBytes };
