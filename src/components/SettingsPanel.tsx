import { useEffect, useState } from "react";
import { CircleCheck, RefreshCw, ShieldAlert } from "lucide-react";
import { Button } from "./ui/button";
import { AnimatedSegmentedControl } from "./ui/animated-segmented-control";
import { IconButton } from "./ui/icon-button";
import { NativeSelect } from "./ui/native-select";
import { useI18n } from "../lib/i18n";
import { useTheme } from "../lib/theme-context";
import { getDiskCacheSettings, setDiskCacheLimit, getCleanupDisposition, setCleanupDisposition, type CleanupDispositionSettings, type CleanupDisposition, type DiskCacheSettings } from "../lib/disk-api";
import { UpdateSettings } from "./UpdateSettings";
import type { DiskPermissionInfo } from "../lib/permissions";
import "../settings.css";
export function SettingsPanel({
  info,
  checking,
  scanActive,
  onCheck,
  onSetup,
}: {
  info: DiskPermissionInfo | null;
  checking: boolean;
  scanActive: boolean;
  onCheck: () => void;
  onSetup: () => void;
}) {
  const { locale, setLocale } = useI18n();
  const { theme, setTheme } = useTheme();
  const tr = (zh: string, en: string) => (locale === "zh" ? zh : en);
  const [cache, setCache] = useState<DiskCacheSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [cacheError, setCacheError] = useState<"read" | "save" | null>(null);
  const [readAttempt, setReadAttempt] = useState(0);
  const [disposition, setDisposition] = useState<CleanupDispositionSettings | null>(null);
  const [dispositionSaving, setDispositionSaving] = useState(false);
  const [dispositionError, setDispositionError] = useState<"read" | "save" | null>(null);
  const [dispositionAttempt, setDispositionAttempt] = useState(0);
  useEffect(() => {
    let cancelled = false;
    getCleanupDisposition().then((settings) => {
      if (!cancelled) { setDisposition(settings); setDispositionError(null); }
    }).catch(() => { if (!cancelled) setDispositionError("read"); });
    return () => { cancelled = true; };
  }, [dispositionAttempt]);
  async function changeDisposition(mode: CleanupDisposition) {
    setDispositionSaving(true);
    setDispositionError(null);
    try { setDisposition(await setCleanupDisposition(mode)); }
    catch {
      setDispositionError("save");
      try { setDisposition(await getCleanupDisposition()); }
      catch { setDisposition(null); setDispositionError("read"); }
    }
    finally { setDispositionSaving(false); }
  }
  useEffect(() => {
    if (saving) return;
    let cancelled = false;
    getDiskCacheSettings().then(
      (settings) => {
        if (cancelled) return;
        setCache(settings);
        setCacheError((error) => error === "read" ? null : error);
      },
      () => { if (!cancelled) setCacheError("read"); },
    );
    return () => { cancelled = true; };
  }, [scanActive, saving, readAttempt]);
  async function changeCacheLimit(value: string) {
    setSaving(true);
    setCacheError(null);
    try {
      setCache(await setDiskCacheLimit(Number(value)));
    } catch {
      setCacheError("save");
    } finally {
      setSaving(false);
    }
  }
  const cacheMiB = (cache?.usedBytes ?? 0) / 1024 ** 2;
  const used = cacheMiB > 0 && cacheMiB < 0.1 ? "< 0.1" : cacheMiB.toLocaleString(locale, { maximumFractionDigits: 1 });
  const granted = info?.status === "granted";
  const blocked = info?.status === "denied";
  return (
    <div>
      <h2 className="settings-heading" id="general-settings">{tr("常规", "General")}</h2>
      <section className="settings-group" aria-labelledby="general-settings">
        <div className="setting-row">
          <strong>{tr("语言", "Language")}</strong>
          <div className="setting-value">
            <NativeSelect
              value={locale}
              onValueChange={(value) => setLocale(value as "zh" | "en")}
              aria-label={tr("语言", "Language")}
            >
              <option value="zh">简体中文</option>
              <option value="en">English</option>
            </NativeSelect>
          </div>
        </div>
        <div className="setting-row">
          <strong>{tr("外观", "Appearance")}</strong>
          <AnimatedSegmentedControl className="setting-value" value={theme} onValueChange={setTheme} ariaLabel={tr("外观", "Appearance")}
            items={[{ value: "system", label: tr("跟随系统", "System") }, { value: "light", label: tr("浅色", "Light") }, { value: "dark", label: tr("深色", "Dark") }]} />
        </div>
      </section>
      <h2 className="settings-heading" id="storage-settings">{tr("存储", "Storage")}</h2>
      <section className="settings-group" aria-labelledby="storage-settings">
        <div className="setting-row">
          <div>
            <strong>{tr("清理方式", "Cleanup action")}</strong>
            <p>{dispositionError === "read" ? tr("无法读取清理方式，卸载前请重试。", "Couldn’t read the cleanup action. Retry before uninstalling.")
              : disposition?.mode === "trash" ? tr("移入废纸篓，清空后才会释放空间。", "Moves items to Trash. Empty it to free space.")
              : disposition ? tr("直接永久删除所选项目，无法从废纸篓恢复。", "Permanently deletes selected items. They cannot be restored from Trash.")
              : tr("正在读取清理方式…", "Reading cleanup action…")}</p>
            {dispositionError === "save" && <p role="alert">{tr("未能确认保存结果，请核对当前方式后重试。", "Couldn’t confirm the save. Check the current action and retry.")}</p>}
          </div>
          <div className="setting-value">
            {dispositionError === "read" && <Button variant="outline" onClick={() => setDispositionAttempt(value => value + 1)}>{tr("重试", "Retry")}</Button>}
            <NativeSelect value={disposition?.mode ?? ""} disabled={(!disposition && dispositionError !== "read") || dispositionSaving}
                onValueChange={value => void changeDisposition(value as CleanupDisposition)} aria-label={tr("清理方式", "Cleanup action")}>
                <option value="" disabled>{dispositionError === "read" ? tr("选择处理方式", "Choose a cleanup action") : tr("读取中…", "Loading…")}</option>
                <option value="permanent">{tr("直接删除（默认）", "Delete permanently (default)")}</option>
                <option value="trash">{tr("移到废纸篓", "Move to Trash")}</option>
            </NativeSelect>
          </div>
        </div>
        <div className="setting-row">
          <div>
            <strong>{tr("磁盘索引缓存", "Disk index cache")}</strong>
            <p aria-live="polite">
              {cacheError === "read"
                ? tr("无法读取当前缓存占用", "Cache usage unavailable")
                : cache
                  ? tr(`已用 ${used} MiB / ${cache.limitMiB} MiB`, `${used} MiB used / ${cache.limitMiB} MiB`)
                  : tr("正在读取占用…", "Reading usage…")}
            </p>
            {cacheError === "save" && <p role="alert">{tr("缓存上限设置失败", "Couldn’t change the cache limit")}</p>}
          </div>
          <div className="setting-value">
            {cacheError === "read" ? (
              <Button variant="outline" onClick={() => setReadAttempt((attempt) => attempt + 1)}>{tr("重试", "Retry")}</Button>
            ) : (
              <NativeSelect
                value={cache ? String(cache.limitMiB) : ""}
                disabled={!cache || saving}
                onValueChange={(value) => void changeCacheLimit(value)}
                aria-label={tr("磁盘索引缓存上限", "Disk index cache limit")}
              >
                <option value="" disabled>{tr("缓存上限", "Cache limit")}</option>
                {[64, 256, 512].map((limit) => <option key={limit} value={String(limit)}>{limit} MiB</option>)}
              </NativeSelect>
            )}
          </div>
        </div>
        <div className="setting-row permission-setting">
          <div>
            <strong>{tr("磁盘访问", "Disk access")}</strong>
            {!granted && (
              <p>
                {checking
                  ? tr(
                      "正在检查当前应用的访问能力…",
                      "Checking this app’s access…",
                    )
                  : blocked
                    ? tr(
                        "更改系统权限后，请重新打开应用。",
                        "Reopen the app after changing system access.",
                      )
                    : tr(
                        "暂时无法确认，可重新检查或前往系统设置。",
                        "Check again or review System Settings.",
                      )}
              </p>
            )}
          </div>
          <div className="setting-value">
            <span
              className={`permission-readiness ${granted ? "is-ready" : blocked ? "is-blocked" : ""}`}
              title={tr(
                "检查当前应用对受保护位置的实际访问能力",
                "Checks this running app’s access to a protected location",
              )}
            >
              {granted ? (
                <CircleCheck size={15} />
              ) : blocked ? (
                <ShieldAlert size={15} />
              ) : null}
              {checking && !info
                ? tr("检查中", "Checking")
                : granted
                  ? tr("已可用", "Available")
                  : blocked
                    ? tr("访问受限", "Restricted")
                    : tr("无法确认", "Unknown")}
            </span>
            {!granted && (
              <>
                <IconButton
                  label={tr("重新检查权限", "Check access again")}
                  disabled={checking}
                  onClick={onCheck}
                >
                  <RefreshCw size={14} className={checking ? "spin" : ""} />
                </IconButton>
                <Button variant="outline" onClick={onSetup}>
                  {tr("去设置", "Set up")}
                </Button>
              </>
            )}
          </div>
        </div>
      </section>
      <UpdateSettings />
      <details className="settings-notes">
        <summary>{tr("扫描与数据说明", "Scanning and data")}</summary>
        <p>
          {tr(
            "扫描只读取文件元数据，不下载云端文件。目录索引缓存达到上限时会淘汰旧记录；应用图标仅缓存在内存。受限路径会列入扫描详情。清理前会重新核对文件，预览会说明本次操作方式。",
            "Scans read metadata without downloading cloud files. The disk index evicts older snapshots when it reaches its limit; app icons stay in memory. Inaccessible paths appear in scan details. Cleanup rechecks files; each preview states its action.",
          )}
        </p>
      </details>
    </div>
  );
}
