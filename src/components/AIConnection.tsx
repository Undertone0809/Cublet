import { useEffect, useRef, useState } from "react";
import { Check, Copy, Link2, LoaderCircle, RefreshCw } from "lucide-react";
import { Button } from "./ui/button";
import { Dialog, DialogBody, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "./ui/dialog";
import "../ai-connection.css";

type ConnectionSetup = {
  name: "cublet-cleaner";
  transport: "stdio";
  command: string;
  args: string[];
  prompt: string;
};

type ActivityEntry = {
  id: string;
  completedAtMs: number;
  deletedAllocatedBytes: number;
};

type ActivityResult = {
  entries: ActivityEntry[];
  skippedRecords?: number;
  truncated?: boolean;
};

function allocatedSize(bytes: number, locale: "zh" | "en") {
  if (bytes === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const exponent = Math.max(0, Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1));
  const value = new Intl.NumberFormat(locale === "zh" ? "zh-CN" : "en-US", {
    maximumFractionDigits: exponent === 0 ? 0 : 1,
  }).format(bytes / 1024 ** exponent);
  return `${value} ${units[exponent]}`;
}

export function AIConnection({ locale }: { locale: "zh" | "en" }) {
  const tr = (zh: string, en: string) => locale === "zh" ? zh : en;
  const native = Boolean(window.electronAPI);
  const [setupOpen, setSetupOpen] = useState(false);
  const setupButton = useRef<HTMLButtonElement>(null);
  const [connectionSetup, setConnectionSetup] = useState<ConnectionSetup | null>(null);
  const [promptState, setPromptState] = useState<"loading" | "ready" | "error">("loading");
  const [promptAttempt, setPromptAttempt] = useState(0);
  const [copyState, setCopyState] = useState<"idle" | "copying" | "copied" | "error">("idle");
  const [entries, setEntries] = useState<ActivityEntry[]>([]);
  const [skippedRecords, setSkippedRecords] = useState(0);
  const [activityTruncated, setActivityTruncated] = useState(false);
  const [activityBusy, setActivityBusy] = useState(false);
  const [activityError, setActivityError] = useState(false);
  const [activityLoaded, setActivityLoaded] = useState(false);
  const [activityAttempt, setActivityAttempt] = useState(0);
  const mounted = useRef(true);
  const copyRequest = useRef(0);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  useEffect(() => {
    if (!native || !setupOpen) return;
    let cancelled = false;
    setPromptState("loading");
    setCopyState("idle");
    copyRequest.current += 1;
    void window.electronAPI!.invoke<ConnectionSetup>("get_agent_connection", { locale }).then((result) => {
      if (cancelled) return;
      if (result?.name !== "cublet-cleaner" || result.transport !== "stdio"
        || typeof result.command !== "string" || !result.command.trim()
        || !Array.isArray(result.args) || !result.args.every((arg) => typeof arg === "string")
        || typeof result.prompt !== "string" || !result.prompt.trim()) throw new Error("Invalid connection prompt");
      setConnectionSetup(result);
      setPromptState("ready");
    }).catch(() => { if (!cancelled) setPromptState("error"); });
    return () => { cancelled = true; };
  }, [native, setupOpen, promptAttempt, locale]);

  useEffect(() => {
    if (!native) return;
    let cancelled = false;
    let pending = false;
    async function readActivity() {
      if (cancelled || pending) return;
      pending = true;
      setActivityBusy(true);
      setActivityError(false);
      try {
        const result = await window.electronAPI!.invoke<ActivityResult>("get_agent_activity");
        if (cancelled) return;
        if (!Array.isArray(result?.entries)) throw new Error("Invalid activity response");
        const validEntries = result.entries.filter((entry) => entry && typeof entry.id === "string"
          && Number.isFinite(entry.completedAtMs) && entry.completedAtMs > 0
          && Number.isFinite(new Date(entry.completedAtMs).getTime())
          && Number.isFinite(entry.deletedAllocatedBytes) && entry.deletedAllocatedBytes >= 0);
        setEntries(validEntries);
        setSkippedRecords(Math.max(0, Number.isFinite(result.skippedRecords) ? result.skippedRecords! : 0) + result.entries.length - validEntries.length);
        setActivityTruncated(result.truncated === true);
        setActivityLoaded(true);
      } catch {
        if (!cancelled) setActivityError(true);
      } finally {
        pending = false;
        if (!cancelled) setActivityBusy(false);
      }
    }
    const refreshForeground = () => {
      if (document.visibilityState === "visible" && document.hasFocus()) void readActivity();
    };
    void readActivity();
    window.addEventListener("focus", refreshForeground);
    document.addEventListener("visibilitychange", refreshForeground);
    return () => {
      cancelled = true;
      window.removeEventListener("focus", refreshForeground);
      document.removeEventListener("visibilitychange", refreshForeground);
    };
  }, [native, activityAttempt]);

  async function copyPrompt() {
    if (!native || promptState !== "ready" || copyState === "copying") return;
    const request = ++copyRequest.current;
    setCopyState("copying");
    try {
      await window.electronAPI!.invoke("copy_agent_connection_prompt", { locale });
      if (mounted.current && request === copyRequest.current) setCopyState("copied");
    } catch {
      if (mounted.current && request === copyRequest.current) setCopyState("error");
    }
  }

  const dateFormat = new Intl.DateTimeFormat(locale === "zh" ? "zh-CN" : "en-US", {
    year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
  });
  return (
    <div className="ai-connection" data-ai-connection>
      {!native ? (
        <section className="ai-connection-state" data-ai-state="unavailable" role="status">
          <Link2 size={23} aria-hidden="true" />
          <h2>{tr("请在 Cublet 桌面应用中打开", "Open this in the Cublet desktop app")}</h2>
          <p>{tr("连接配置和清理记录由本机应用提供。", "Connection setup and cleanup history are provided by the local app.")}</p>
        </section>
      ) : (
        <>
          <section className="ai-connection-panel" aria-labelledby="ai-setup-title" data-ai-panel="setup">
            <div className="ai-connection-panel-heading ai-connection-setup">
              <div>
                <h2 id="ai-setup-title">Cublet Cleaner</h2>
                <p>{tr("让你的 Agent 使用 Cublet 清理磁盘。", "Let your agent use Cublet to clean up disk space.")}</p>
              </div>
              <Button size="sm" ref={setupButton} onClick={() => setSetupOpen(true)}>
                <Link2 size={14} aria-hidden="true" />{tr("连接说明", "Connection instructions")}
              </Button>
            </div>
          </section>
          <section className="ai-connection-panel" aria-labelledby="ai-activity-title" data-ai-panel="activity">
            <div className="ai-connection-panel-heading">
              <h2 id="ai-activity-title">{tr("最近的本地清理记录", "Recent local cleanups")}</h2>
              <Button variant="ghost" size="sm" disabled={activityBusy} onClick={() => setActivityAttempt((value) => value + 1)} aria-label={tr("刷新清理记录", "Refresh cleanup history")} data-ai-refresh>
                <RefreshCw size={13} className={activityBusy ? "spin" : ""} aria-hidden="true" />{tr("刷新", "Refresh")}
              </Button>
            </div>
            {activityError && (
              <div className="ai-connection-inline-error" data-ai-state="activity-error">
                <span role="alert">{tr("暂时无法读取清理记录。", "Couldn’t read the cleanup history.")}</span>
                <Button variant="outline" size="sm" onClick={() => setActivityAttempt((value) => value + 1)}>{tr("重试", "Retry")}</Button>
              </div>
            )}
            {!activityLoaded && !activityError ? (
              <div className="ai-connection-state" role="status" data-ai-state="activity-loading"><LoaderCircle size={19} className="spin" aria-hidden="true" /><p>{tr("正在读取清理记录…", "Loading cleanup history…")}</p></div>
            ) : activityLoaded && entries.length === 0 && !activityError ? (
              <div className="ai-connection-state" role="status" data-ai-state="activity-empty"><h3>{skippedRecords > 0 ? tr("暂无可显示的记录", "No readable records") : tr("还没有清理记录", "No cleanup history yet")}</h3><p>{skippedRecords > 0 ? tr("部分本地记录未能读取。", "Some local records couldn’t be read.") : tr("Agent 完成缓存清理后，记录会显示在这里。", "Completed agent cache cleanups will appear here.")}</p></div>
            ) : entries.length > 0 ? (
              <div className="ai-connection-history" aria-busy={activityBusy}>
                <table>
                  <thead><tr><th scope="col">{tr("清理时间", "Cleanup time")}</th><th scope="col">{tr("已清理空间", "Space cleaned")}</th></tr></thead>
                  <tbody>{entries.map((entry) => <tr key={entry.id}><td><time dateTime={new Date(entry.completedAtMs).toISOString()}>{dateFormat.format(entry.completedAtMs)}</time></td><td>{allocatedSize(entry.deletedAllocatedBytes, locale)}</td></tr>)}</tbody>
                </table>
              </div>
            ) : null}
            {skippedRecords > 0 && <p className="ai-connection-incomplete" role="status" data-ai-incomplete>{tr("部分记录无法读取，当前列表可能不完整。", "Some records couldn’t be read. This list may be incomplete.")}</p>}
            {activityTruncated && <p className="ai-connection-incomplete" data-ai-retention>{tr("仅显示最近保留的本地记录。", "Only the most recent retained local records are shown.")}</p>}
          </section>
        </>
      )}
      <Dialog open={setupOpen} onOpenChange={setSetupOpen}>
        <DialogContent className="ai-connection-setup-dialog" closeLabel={tr("关闭", "Close")} finalFocus={() => setupButton.current ?? true} data-ai-setup-dialog>
          <DialogHeader>
            <DialogTitle>Cublet Cleaner</DialogTitle>
            <DialogDescription>{tr("复制给 Agent，让它完成 MCP 连接。", "Copy to your agent to set up the MCP connection.")}</DialogDescription>
          </DialogHeader>
          <DialogBody>
            {promptState === "loading" ? (
              <div className="ai-connection-prompt-state" role="status" data-ai-state="prompt-loading"><LoaderCircle size={17} className="spin" aria-hidden="true" />{tr("正在读取连接说明…", "Loading connection instructions…")}</div>
            ) : promptState === "error" ? (
              <div className="ai-connection-prompt-state" data-ai-state="prompt-error"><p role="alert">{tr("暂时无法读取连接说明。", "Couldn’t load the connection instructions.")}</p><Button variant="outline" size="sm" onClick={() => setPromptAttempt((value) => value + 1)}><RefreshCw size={14} />{tr("重试", "Retry")}</Button></div>
            ) : (
              <>
                <div className="ai-connection-prompt-heading"><h3 id="ai-prompt-title">{tr("连接说明", "Connection instructions")}</h3><span data-ai-transport>{tr("本地 stdio", "Local stdio")}</span></div>
                <div className="ai-connection-prompt" data-ai-prompt tabIndex={0} aria-labelledby="ai-prompt-title">{connectionSetup?.prompt}</div>
                <div className="ai-connection-copy-row">
                  <Button onClick={() => void copyPrompt()} disabled={copyState === "copying"} data-ai-copy>
                    {copyState === "copied" ? <Check size={15} /> : copyState === "copying" ? <LoaderCircle size={15} className="spin" /> : <Copy size={15} />}
                    {copyState === "copied" ? tr("已复制", "Copied") : copyState === "copying" ? tr("正在复制…", "Copying…") : copyState === "error" ? tr("重试复制", "Try copying again") : tr("复制给 Agent", "Copy for agent")}
                  </Button>
                  <span className={copyState === "error" ? "ai-connection-copy-error" : "ai-connection-copy-note"} role={copyState === "error" ? "alert" : "status"}>{copyState === "error" ? tr("复制失败，请重试。", "Copy failed. Please try again.") : copyState === "copied" ? tr("已复制，粘贴到 Agent 对话中。", "Copied. Paste into your agent’s conversation.") : ""}</span>
                </div>
              </>
            )}
          </DialogBody>
        </DialogContent>
      </Dialog>
    </div>
  );
}

export default AIConnection;
