const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');

const FULL_DISK_ACCESS_SETTINGS = 'x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles';
// Darwin fcntl.h: reject symlinks in every path component. Node does not expose
// this flag. It is used only behind the Darwin guard below.
const DARWIN_O_NOFOLLOW_ANY = 0x20000000;

// Full Disk Access has no public preflight API. This sentinel is a heuristic for
// effective access, not an API to read the System Settings FDA switch. Its path
// and protection can change across macOS versions. Apple DTS explicitly cautions
// against treating it as an API: developer.apple.com/forums/thread/114452.
// Test effective access in the
// Electron main process itself: a child launched from a terminal can inherit a
// different responsible app's permission. Only open/close the user's TCC sentinel;
// never read its contents, enumerate other apps' data, or probe the folder consent
// categories (Desktop/Documents/Downloads). The AllFiles gate does not present a
// folder-consent dialog. macOS may add an unchecked entry to its FDA settings.
// Unlike the system TCC database, the user's database avoids root-owned parent
// directories causing false denials. Neither a readable nor a blocked sentinel
// proves that every other protected file is readable or that FDA is toggled off.
function createDiskPermissions({ app, shell, platform = process.platform, fileSystem = fs,
  userHome = () => os.userInfo().homedir, now = () => new Date() }) {
  let settingsOpened = false;
  function appBundlePath() {
    const executable = app.getPath('exe');
    let candidate = executable;
    while (candidate !== path.dirname(candidate)) {
      if (candidate.toLowerCase().endsWith('.app')) return candidate;
      candidate = path.dirname(candidate);
    }
    return executable;
  }
  function probe() {
    if (platform !== 'darwin') return { status: 'unknown', result: 'unsupported' };
    let fd;
    try {
      const sentinel = path.join(userHome(), 'Library', 'Application Support', 'com.apple.TCC', 'TCC.db');
      // Reject redirected parent paths too, and avoid blocking on a substituted
      // FIFO before fstat rejects non-regular files. Ordinary files are unaffected.
      fd = fileSystem.openSync(sentinel, fs.constants.O_RDONLY | fs.constants.O_NONBLOCK | DARWIN_O_NOFOLLOW_ANY);
      if (!fileSystem.fstatSync(fd).isFile()) return { status: 'unknown', result: 'unavailable' };
      return { status: 'granted', result: 'readable' };
    } catch (error) {
      // EPERM proves this probe is blocked, not which macOS policy denied it.
      if (error.code === 'EPERM') return { status: 'denied', result: 'blocked', code: 'EPERM' };
      // POSIX/ACL failure is not evidence that the FDA setting is disabled.
      if (error.code === 'EACCES') return { status: 'unknown', result: 'filesystem-denied', code: 'EACCES' };
      if (['ENOENT', 'ENOTDIR', 'ELOOP'].includes(error.code)) {
        return { status: 'unknown', result: 'unavailable', code: error.code };
      }
      return { status: 'unknown', result: 'error' };
    } finally {
      if (fd !== undefined) {
        try { fileSystem.closeSync(fd); } catch { /* Closing cannot change the observed open result. */ }
      }
    }
  }
  function getDiskPermissions(_options = {}) {
    // Deliberately no cache: refresh and ordinary checks both observe current
    // effective access after Settings changes, rather than a saved UI choice.
    const observed = probe();
    const details = observed.status === 'granted' ? [
      '当前运行的应用已通过访问检查，可以打开系统保护的检测文件；不会读取文件内容。',
      '此结果反映当前进程对检测文件的实际访问能力，不直接读取系统设置中的授权开关；其他文件仍可能受到限制。',
    ] : observed.status === 'denied' ? [
      'macOS 拒绝了当前应用访问系统保护的检测文件，当前进程的访问受到限制。',
      '请在系统设置中检查此应用的完整磁盘访问权限；如果已经启用，可退出并重新打开应用后检查。其他系统策略也可能造成访问受限。',
    ] : [
      observed.result === 'filesystem-denied'
        ? '检测文件受到文件系统权限限制，无法据此判断完整磁盘访问权限是否启用。'
        : '当前无法使用系统保护的检测文件确认完整磁盘访问权限，请在系统设置中检查。',
      '打开设置或确认继续不代表系统已经授权；修改权限后可能需要退出并重新打开应用。',
    ];
    if (!app.isPackaged) details.push('开发模式检查的是 Electron 的实际访问能力，请使用已安装应用确认正式应用权限。');
    return {
      status: observed.status,
      canVerify: observed.status !== 'unknown',
      checkedAt: now().toISOString(),
      evidence: { kind: 'full-disk-access-sentinel', result: observed.result,
        ...(observed.code ? { code: observed.code } : {}) },
      platform,
      appPath: appBundlePath(),
      appName: app.isPackaged ? app.getName() : 'Electron（开发模式）',
      isPackaged: app.isPackaged,
      relaunchRecommended: settingsOpened && observed.status !== 'granted',
      details,
    };
  }
  async function openDiskPrivacySettings() {
    if (platform !== 'darwin') return { opened: false, relaunchRecommended: false };
    try {
      await shell.openExternal(FULL_DISK_ACCESS_SETTINGS);
      settingsOpened = true;
      return { opened: true, relaunchRecommended: true };
    } catch {
      return { opened: false, relaunchRecommended: false };
    }
  }
  return { getDiskPermissions, openDiskPrivacySettings };
}

module.exports = { createDiskPermissions, FULL_DISK_ACCESS_SETTINGS };
