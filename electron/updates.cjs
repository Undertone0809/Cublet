// Renderer requests are commands only. The release host/channel/architecture stay native-owned.
const UPDATE_ORIGIN = 'https://downloads.cublet.cc';
function updateFeed(arch) {
  if (!['arm64', 'x64'].includes(arch)) throw new Error('Unsupported update architecture');
  return `${UPDATE_ORIGIN}/updates/stable/${arch}/`;
}
function updateError(error) {
  const text = String(error?.code ?? '') + ' ' + String(error?.message ?? error);
  if (/ERR_(INTERNET_DISCONNECTED|NAME_NOT_RESOLVED|CONNECTION|NETWORK)|ENOTFOUND|ETIMEDOUT|ECONN|EAI_AGAIN|net::/i.test(text)) return 'offline';
  if (/signature|codesign|code sign|identity|certificate|designated requirement/i.test(text)) return 'signature';
  return 'failed';
}
function createUpdateController({ updater, nativeUpdater, version, enabled, prepareInstall, onChange = () => {} }) {
  let state = { status: enabled ? 'idle' : 'unavailable', currentVersion: version, latestVersion: null, percent: 0, error: null, retry: 'check', checkedAt: null };
  let operation;
  let operationKind;
  let installAttempt = 0;
  const snapshot = () => ({ ...state });
  const publish = patch => { state = { ...state, ...patch }; onChange(snapshot()); };
  let installListeners = [];
  function clearInstallListeners() {
    // electron-updater 6.8.11 registers native install callbacks with `on`, even
    // when signature/network validation fails. Remove only callbacks added by
    // our failed install, preserving Electron/updater's own readiness listeners.
    for (const listener of installListeners) nativeUpdater?.removeListener('update-downloaded', listener);
    installListeners = [];
  }
  const fail = (error, retry = operationKind ?? 'check') => {
    installAttempt += 1;
    if (operation?.kind === 'install') operation = undefined;
    clearInstallListeners();
    publish({ status: 'error', error: updateError(error), retry });
  };
  if (enabled) {
    updater.autoDownload = false;
    updater.autoInstallOnAppQuit = false;
    updater.allowDowngrade = false;
    updater.allowPrerelease = false;
    updater.on('checking-for-update', () => publish({ status: 'checking', error: null }));
    updater.on('update-available', info => publish({ status: 'available', latestVersion: info.version, checkedAt: Date.now(), error: null, percent: 0 }));
    updater.on('update-not-available', info => publish({ status: 'current', latestVersion: info.version, checkedAt: Date.now(), error: null }));
    updater.on('download-progress', info => publish({ status: 'downloading', percent: Math.max(0, Math.min(100, Number(info.percent) || 0)) }));
    updater.on('update-downloaded', info => publish({ status: 'ready', latestVersion: info.version, percent: 100, error: null }));
    updater.on('error', error => fail(error));
  }
  async function run(kind, action) {
    if (!enabled || operation) return snapshot();
    operationKind = kind;
    const owner = { kind };
    operation = owner;
    // A failed native install releases its command while an old preparation may
    // still be settling. That abandoned promise must never clear a newer owner.
    owner.promise = Promise.resolve().then(action).catch(error => {
      if (operation === owner) fail(error, kind);
    }).finally(() => { if (operation === owner) operation = undefined; });
    await owner.promise;
    return snapshot();
  }
  return {
    snapshot,
    check() {
      if (['ready', 'installing', 'downloading', 'available'].includes(state.status)) return Promise.resolve(snapshot());
      return run('check', async () => {
        publish({ status: 'checking', error: null });
        const result = await updater.checkForUpdates();
        if (result === null) publish({ status: 'unavailable' });
      });
    },
    download() {
      if (!(state.status === 'available' || (state.status === 'error' && state.retry === 'download'))) return Promise.resolve(snapshot());
      return run('download', async () => {
        publish({ status: 'downloading', percent: 0, error: null });
        await updater.downloadUpdate();
      });
    },
    install() {
      if (!(state.status === 'ready' || (state.status === 'error' && state.retry === 'install'))) return Promise.resolve(snapshot());
      return run('install', async () => {
        const attempt = ++installAttempt;
        const isCurrent = () => attempt === installAttempt && state.status === 'installing';
        publish({ status: 'installing', error: null });
        await prepareInstall({ isCurrent });
        if (!isCurrent()) return;
        // Squirrel.Mac verifies the update against the running app's designated code
        // requirement before installing. Never bypass its signature validation.
        const before = nativeUpdater?.listeners('update-downloaded') ?? [];
        try { updater.quitAndInstall(); }
        finally {
          installListeners = (nativeUpdater?.listeners('update-downloaded') ?? []).filter(listener => !before.includes(listener));
          if (state.status === 'error') clearInstallListeners();
        }
      });
    },
  };
}
module.exports = { createUpdateController, updateFeed, updateError };
