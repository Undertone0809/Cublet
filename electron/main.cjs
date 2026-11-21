const { app, BrowserWindow, ipcMain, shell, nativeImage, nativeTheme, clipboard } = require('electron');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

// Product branding must not create a new profile or permission/onboarding history.
require('./app-identity.cjs').configureAppIdentity(app);
const { createUpdateController, updateFeed } = require('./updates.cjs');
const { checkpointScans, prepareUpdateShutdown, createMutationTracker } = require('./update-shutdown.cjs');
const { connectionDetails } = require('./agent-connection.cjs');

const {createEntryActions} = require('./file-actions.cjs');
const {createDiskPermissions} = require('./permissions.cjs');
const {createAppIconStore} = require('./app-icons.cjs');
const { createApplicationService } = require('./application-service.cjs');
const { createUninstallInventory } = require('./uninstall-inventory.cjs');
const diskPermissions = createDiskPermissions({app, shell});
const appIcons = createAppIconStore({app, nativeImage});
const appUninstaller = createApplicationService({ app, shell, invokeNative: invokeCore });
const uninstallInventory = createUninstallInventory(app);
const entryActions = createEntryActions({ shell });
const appMeasurements = new Map();

const RPC_METHODS = new Set([
  'get_agent_activity',
  'disk_overview', 'start_disk_scan', 'get_disk_scan', 'get_disk_scan_view', 'cancel_disk_scan',
  'inspect_disk_directory', 'list_disk_directory', 'get_cached_disk_scan',
  'get_disk_cache_settings', 'set_disk_cache_limit', 'get_disk_scan_comparison', 'list_disk_scan_history',
  'scan_storage_fast', 'restore_live_cleanup_operation', 'list_live_cleanup_operations', 'purge_live_cleanup_operation',
  'scan_storage',
  'list_worktrees',
  'remove_worktree',
  'remove_worktrees',
  'get_codex_status',
  'quit_codex',
  'create_cleanup_plan',
  'execute_cleanup_plan',
  'restore_cleanup_operation',
  'create_compaction_plan',
  'execute_compaction_plan',
  'restore_compaction_operation',
  'list_cleanup_operations',
  'purge_recovery',
  'create_managed_cleanup_plan',
  'execute_managed_cleanup_plan',
  'restore_managed_cleanup_operation',
]);

let mainWindow;
let coreProcess;
const activeScanIds = new Set();
let stdoutBuffer = '';
let nextRequestId = 1;
let quitPending = false;
let uninstallInFlight;
let updateShutdownPending = false;
let updateController;
const mutationTracker = createMutationTracker();
const pending = new Map();
// Mutations must resolve from the backend: a UI timeout must never imply they stopped.
const MUTATIONS = /^(execute_|dispose_|remove_|restore_|purge_|quit_|set_disk_cache_limit$|set_cleanup_disposition$)/;
const PRIVATE_APPLICATION_RPC = new Set(['get_cleanup_disposition', 'set_cleanup_disposition', 'seal_application_item', 'dispose_application_item']);

if (process.env.ELECTRON_REMOTE_DEBUGGING_PORT) {
  app.commandLine.appendSwitch('remote-debugging-port', process.env.ELECTRON_REMOTE_DEBUGGING_PORT);
}

function corePath() {
  if (app.isPackaged) return path.join(process.resourcesPath, 'bin', 'codex-core');
  if (process.env.CODEX_CORE_PATH) return process.env.CODEX_CORE_PATH;
  const release = path.join(__dirname, '..', 'rust-core', 'target', 'release', 'codex-core');
  const debug = path.join(__dirname, '..', 'rust-core', 'target', 'debug', 'codex-core');
  return fs.existsSync(release) ? release : debug;
}

function childEnvironment() {
  const env = { ...process.env };
  delete env.ELECTRON_RUN_AS_NODE;
  return env;
}

function rejectPending(error) {
  for (const request of pending.values()) request.reject(error);
  pending.clear();
}

function handleCoreLine(line) {
  let message;
  try {
    message = JSON.parse(line);
  } catch (error) {
    console.error('[codex-core] invalid response:', error, line);
    return;
  }
  const request = pending.get(String(message.id));
  if (!request) return;
  pending.delete(String(message.id));
  if (message.ok) request.resolve(message.result);
  else request.reject(new Error(message.error || 'Rust core request failed'));
}

function startCore() {
  if (coreProcess && !coreProcess.killed) return coreProcess;
  const executable = corePath();
  if (!fs.existsSync(executable)) {
    throw new Error(`Rust core was not built: ${executable}`);
  }
  coreProcess = spawn(executable, [], {
    cwd: path.dirname(executable),
    env: childEnvironment(),
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  stdoutBuffer = '';
  coreProcess.stdout.setEncoding('utf8');
  coreProcess.stdout.on('data', (chunk) => {
    stdoutBuffer += chunk;
    const lines = stdoutBuffer.split('\n');
    stdoutBuffer = lines.pop() || '';
    for (const line of lines) if (line.trim()) handleCoreLine(line);
  });
  coreProcess.stderr.setEncoding('utf8');
  coreProcess.stderr.on('data', (chunk) => console.error('[codex-core]', chunk.trimEnd()));
  coreProcess.on('error', (error) => {
    rejectPending(error);
    coreProcess = undefined;
    activeScanIds.clear();
  });
  coreProcess.on('exit', (code, signal) => {
    const error = new Error(`Rust core exited (${code ?? 'unknown'}${signal ? `, ${signal}` : ''})`);
    rejectPending(error);
    coreProcess = undefined;
    activeScanIds.clear();
  });
  return coreProcess;
}

function invokeCore(method, params = {}) {
  if (!RPC_METHODS.has(method) && !PRIVATE_APPLICATION_RPC.has(method)) return Promise.reject(new Error(`Unsupported RPC method: ${method}`));
  let processHandle;
  try {
    processHandle = startCore();
  } catch (error) {
    return Promise.reject(error);
  }
  const id = String(nextRequestId++);
  return new Promise((resolve, reject) => {
    const timeout = MUTATIONS.test(method) ? undefined : setTimeout(() => {
      pending.delete(id);
      reject(new Error(`Rust core request timed out: ${method}`));
    }, 60_000);
    pending.set(id, {
      resolve: (value) => {
        clearTimeout(timeout);
        if (['start_disk_scan', 'inspect_disk_directory', 'get_disk_scan', 'cancel_disk_scan'].includes(method)) {
          if (value?.status === 'running') activeScanIds.add(value.scanId);
          else activeScanIds.delete(value?.scanId);
        }
        if (Array.isArray(value?.applications)) {
          Promise.all([
            uninstallInventory.filter(value.applications),
            uninstallInventory.isStale(value.startedAtMs),
          ]).then(([candidates, usageStale]) => {
            appUninstaller.rememberApplications(candidates, value.applicationCopiesTruncated);
            const applications = appUninstaller.listApplications(candidates, value.viewPath);
            if (!usageStale && value.status === 'complete') {
              for (const measured of applications) {
                appMeasurements.delete(measured.path);
                appMeasurements.set(measured.path, measured);
              }
              while (appMeasurements.size > 4096) appMeasurements.delete(appMeasurements.keys().next().value);
            }
            appIcons.rememberApplications(applications);
            resolve({ ...value, applications, usageStale });
          }).catch(reject);
        } else resolve(value);
      },
      reject: (error) => { clearTimeout(timeout); reject(error); },
    });
    try {
      processHandle.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
    } catch (error) {
      clearTimeout(timeout);
      pending.delete(id);
      reject(error);
    }
  });
}

function createWindow() {
  const windowStyle = process.platform === 'darwin' ? {
    titleBarStyle: 'hiddenInset',
    trafficLightPosition: { x: 18, y: 17 },
    vibrancy: 'under-window',
    visualEffectState: 'followWindow',
    backgroundColor: '#00000000',
  } : { backgroundColor: '#0b0b0b' };
  mainWindow = new BrowserWindow({
    width: 1180,
    height: 780,
    minWidth: 900,
    minHeight: 620,
    title: 'Cublet',
    ...windowStyle,
    autoHideMenuBar: true,
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  mainWindow.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  const startUrl = process.env.ELECTRON_START_URL;
  if (app.isPackaged || !startUrl) {
    mainWindow.loadFile(path.join(__dirname, '..', 'dist', 'index.html'));
  } else {
    mainWindow.loadURL(startUrl);
  }
  mainWindow.on('closed', () => {
    mainWindow = undefined;
    for (const scanId of activeScanIds) void invokeCore('cancel_disk_scan', {scanId}).catch(() => {});
  });
}

function checkUninstallSender(event) {
  const contents = mainWindow?.webContents;
  const frame = event.senderFrame;
  if (!contents || event.sender !== contents || !frame || frame !== contents.mainFrame) {
    throw new Error('Invalid uninstall request origin');
  }
  const devUrl = process.env.ELECTRON_START_URL;
  const expectedFile = pathToFileURL(path.join(__dirname, '..', 'dist', 'index.html')).href;
  if (app.isPackaged || !devUrl) {
    if (frame.url !== expectedFile) throw new Error('Invalid uninstall request origin');
  } else if (new URL(frame.url).origin !== new URL(devUrl).origin) {
    throw new Error('Invalid uninstall request origin');
  }
}

ipcMain.handle('updates:command', (event, command) => {
  checkUninstallSender(event);
  if (!updateController || !['state', 'check', 'download', 'install'].includes(command)) throw new Error('Invalid update command');
  if (command === 'state') return updateController.snapshot();
  return updateController[command]();
});

function handleCoreRequest(event, request) {
  if (!request || typeof request.method !== 'string' || !(RPC_METHODS.has(request.method) || ['get_agent_connection', 'copy_agent_connection_prompt', 'reveal_path', 'get_trash_path', 'get_path_details', 'preview_trash_entry', 'trash_entry', 'get_disk_permissions', 'open_disk_privacy_settings', 'get_app_icon', 'set_window_appearance', 'search_applications', 'preview_app_uninstall', 'execute_app_uninstall', 'get_cleanup_disposition', 'set_cleanup_disposition'].includes(request.method))) {
    throw new Error('Unsupported or malformed Rust core request');
  }
  if (updateShutdownPending && (MUTATIONS.test(request.method) || ['start_disk_scan', 'inspect_disk_directory', 'trash_entry'].includes(request.method))) {
    throw new Error('Cublet is preparing to install an update');
  }
  const params = request.params && typeof request.params === 'object' ? request.params : {};
  if (request.method === 'get_agent_connection' || request.method === 'copy_agent_connection_prompt') {
    checkUninstallSender(event);
    const executable = app.isPackaged
      ? path.join(process.resourcesPath, 'bin', 'cublet-cleaner')
      : corePath();
    const connection = connectionDetails(executable, params.locale);
    if (request.method === 'get_agent_connection') return connection;
    clipboard.writeText(connection.prompt);
    return null;
  }
  if (request.method === 'get_trash_path') { checkUninstallSender(event); return entryActions.trashPath; }
  if (['get_path_details', 'preview_trash_entry', 'trash_entry'].includes(request.method)) {
    checkUninstallSender(event);
    if (request.method === 'get_path_details') return entryActions.details(params.path);
    if (request.method === 'preview_trash_entry') return entryActions.preview(params.path);
    return entryActions.execute(params.planId, params.confirmed, params.action);
  }
  if (request.method === 'set_window_appearance') {
    if (event.sender !== mainWindow?.webContents || !['system', 'light', 'dark'].includes(params.theme)) {
      throw new Error('Invalid window appearance');
    }
    nativeTheme.themeSource = params.theme;
    return null;
  }
  if (request.method === 'preview_app_uninstall') {
    checkUninstallSender(event);
    return appUninstaller.preview(params.paths, params.retryPlanId);
  }
  if (request.method === 'get_cleanup_disposition' || request.method === 'set_cleanup_disposition') {
    checkUninstallSender(event);
    return invokeCore(request.method, { mode: params.mode });
  }
  if (request.method === 'search_applications') {
    checkUninstallSender(event);
    return appUninstaller.search({ query: params.query, refresh: params.refresh === true }).then(async result => {
      const applications = (await uninstallInventory.filter(result.applications)).map(entry => {
        const measured = appMeasurements.get(entry.path);
        return measured?.bundleId && measured.bundleId === entry.bundleId
          ? { ...entry, bundleBytes: entry.bundleBytesKnown ? entry.bundleBytes : measured.bundleBytes, bundleBytesKnown: true,
            supportBytes: measured.supportBytes, cacheBytes: measured.cacheBytes, sharedData: measured.sharedData }
          : entry;
      });
      appIcons.rememberApplications(applications);
      return { ...result, applications };
    });
  }
  if (request.method === 'execute_app_uninstall') {
    checkUninstallSender(event);
    if (params.confirmed !== true) throw new Error('Confirm the selected items and removal method');
    if (uninstallInFlight) throw new Error('An application uninstall is already in progress');
    uninstallInFlight = appUninstaller.execute(params.planId, params.selectedIds, params.mode).then(async (result) => {
      if (result.movedPaths.length) {
        appMeasurements.clear();
        await uninstallInventory.remember(result.movedAppPaths).catch(() => {});
      }
      return result;
    }).finally(() => { uninstallInFlight = undefined; });
    return uninstallInFlight;
  }
  if (request.method === 'get_app_icon') return appIcons.getAppIcon(params);
  if (request.method === 'get_disk_permissions') return diskPermissions.getDiskPermissions(params);
  if (request.method === 'open_disk_privacy_settings') return diskPermissions.openDiskPrivacySettings();
  if (request.method === 'reveal_path') {
    if (typeof params.path !== 'string' || !path.isAbsolute(params.path)) throw new Error('Invalid path');
    shell.showItemInFolder(params.path); return null;
  }
  return invokeCore(request.method, params);
}

ipcMain.handle('core:invoke', (event, request) => {
  const result = handleCoreRequest(event, request);
  return request && (MUTATIONS.test(request.method) || request.method === 'trash_entry')
    ? mutationTracker.track(result) : result;
});

app.whenReady().then(() => {
  const enabled = app.isPackaged && require('../package.json').cubletOfficialRelease === true
    && process.platform === 'darwin' && ['arm64', 'x64'].includes(process.arch);
  const updater = enabled ? new (require(app.isPackaged ? '../electron-runtime/updater.cjs' : 'electron-updater').MacUpdater)({ provider: 'generic', url: updateFeed(process.arch) }) : null;
  updateController = createUpdateController({
    updater, nativeUpdater: require('electron').autoUpdater, version: app.getVersion(), enabled,
    onChange: state => {
      if (state.status === 'error') { updateShutdownPending = false; quitPending = false; }
      if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send('updates:state', state);
    },
    prepareInstall: async ({ isCurrent }) => {
      updateShutdownPending = true;
      await prepareUpdateShutdown({
        waitForUninstall: () => mutationTracker.settle(),
        isCurrent,
        checkpoint: () => checkpointScans([...activeScanIds], scanId => invokeCore('cancel_disk_scan', { scanId })),
      });
      if (isCurrent()) quitPending = true;
    },
  });
  startCore();
  createWindow();
  // One delayed check per process launch; never download or install automatically.
  if (enabled) { const timer = setTimeout(() => void updateController.check(), 10_000); timer.unref(); }
  app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});

app.on('before-quit', (event) => {
  if (uninstallInFlight) {
    event.preventDefault();
    void uninstallInFlight.then(() => app.quit(), () => app.quit());
    return;
  }
  // Let Rust atomically save its small checkpoint before ending a long scan.
  // A timeout keeps shutdown bounded if the native process stops responding.
  if (!quitPending && activeScanIds.size && coreProcess && !coreProcess.killed) {
    event.preventDefault();
    quitPending = true;
    void checkpointScans([...activeScanIds], scanId => invokeCore('cancel_disk_scan', { scanId })).finally(() => app.quit());
    return;
  }
  rejectPending(new Error('Electron application is shutting down'));
  if (coreProcess && !coreProcess.killed) coreProcess.kill();
});
