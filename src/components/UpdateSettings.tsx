import { useEffect, useState } from 'react';
import { version } from '../../package.json';
import { useI18n } from '../lib/i18n';
import type { UpdateCommand, UpdateState } from '../lib/updates';
import { Button } from './ui/button';

const initial: UpdateState = { status: 'unavailable', currentVersion: version, latestVersion: null, percent: 0, error: null, retry: 'check', checkedAt: null };
export function UpdateSettings() {
  const { locale } = useI18n();
  const tr = (zh: string, en: string) => locale === 'zh' ? zh : en;
  const [state, setState] = useState(initial);
  const [commandPending, setCommandPending] = useState(false);
  useEffect(() => {
    const api = window.electronAPI?.updates;
    if (!api) return;
    let live = true;
    let receivedEvent = false;
    const unsubscribe = api.subscribe(next => { receivedEvent = true; if (live) setState(next); });
    void api.command('state').then(next => { if (live && !receivedEvent) setState(next); }).catch(() => {
      if (live) setState(previous => ({ ...previous, status: 'error', error: 'failed', retry: 'check' }));
    });
    return () => { live = false; unsubscribe(); };
  }, []);
  async function command(action: UpdateCommand) {
    const api = window.electronAPI?.updates;
    if (!api || commandPending) return;
    setCommandPending(true);
    try { setState(await api.command(action)); }
    catch { setState(previous => ({ ...previous, status: 'error', error: 'failed', retry: action === 'download' || action === 'install' ? action : 'check' })); }
    finally { setCommandPending(false); }
  }
  const busy = commandPending || ['checking', 'downloading', 'installing'].includes(state.status);
  const message = {
    idle: tr('启动时自动检查。下载和安装由你确认。', 'Checks automatically on launch. You choose when to download and install.'),
    unavailable: tr('请在安装的 macOS 版本中检查更新。', 'Check for updates in the installed macOS app.'),
    checking: tr('正在检查更新…', 'Checking for updates…'),
    current: tr('已是最新版本', 'You’re up to date'),
    available: tr(`新版本 ${state.latestVersion} 可用`, `Version ${state.latestVersion} is available`),
    downloading: tr(`正在下载 · ${Math.round(state.percent)}%`, `Downloading · ${Math.round(state.percent)}%`),
    ready: tr(`${state.latestVersion} 已下载，可以重启安装。`, `${state.latestVersion} is downloaded and ready to install.`),
    installing: tr('正在保存扫描进度并验证更新，随后自动重启…', 'Saving scan progress and verifying the update, then restarting…'),
    error: state.error === 'offline'
      ? tr('无法连接更新服务。检查网络后重试。', 'Can’t reach the update service. Check your connection and retry.')
      : state.error === 'signature'
        ? tr('更新签名验证失败，未安装。请重新打开应用后重试。', 'The update signature could not be verified. Reopen Cublet and try again.')
        : tr('更新失败，请重试。', 'The update failed. Please retry.'),
  }[state.status];
  const action: UpdateCommand = state.status === 'ready' ? 'install' : state.status === 'available' ? 'download' : state.status === 'error' ? state.retry : 'check';
  const label = state.status === 'error' ? tr('重试', 'Retry') : action === 'install' ? tr('重启并安装', 'Restart and install') : action === 'download' ? tr('下载更新', 'Download update') : tr('检查更新', 'Check for updates');
  return <>
    <h2 className="settings-heading" id="update-settings">{tr('软件更新', 'Software update')}</h2>
    <section className="settings-group" aria-labelledby="update-settings">
      <div className="setting-row">
        <div>
          <strong>Cublet · {tr(`当前版本 ${state.currentVersion}`, `Current version ${state.currentVersion}`)}</strong>
          <p role={state.status === 'error' ? 'alert' : 'status'} aria-live="polite">{message}</p>
          {state.status === 'downloading' && <progress className="update-progress" max={100} value={state.percent} aria-label={tr('更新下载进度', 'Update download progress')} />}
        </div>
        <div className="setting-value">
          <Button variant="outline" disabled={busy || state.status === 'unavailable'} onClick={() => void command(action)}>
            {state.status === 'checking' ? tr('检查中…', 'Checking…') : state.status === 'downloading' ? tr('下载中…', 'Downloading…') : state.status === 'installing' ? tr('准备重启…', 'Preparing restart…') : label}
          </Button>
        </div>
      </div>
    </section>
  </>;
}
