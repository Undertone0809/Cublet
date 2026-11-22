import { useEffect, useState } from "react";
import { History, LoaderCircle } from "lucide-react";
import { diskInvoke } from "../lib/disk-api";
import { formatBytes } from "../lib/utils";
import { Button } from "./ui/button";
import { Dialog, DialogBody, DialogContent, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "./ui/dialog";
import { NativeSelect } from "./ui/native-select";

type ScanSummary = { updatedAtMs: number };
type AgentScope = {
  id: string;
  allowProtected: boolean;
  roots: string[];
  current: ScanSummary;
  allocatedBytes: number;
  scannedFiles: number;
  coverageComplete: boolean;
};
type AgentHistory = {
  scopes: AgentScope[];
  selectedScopeId: string | null;
  comparison: {
    baseline: ScanSummary;
    status: string;
    growthBytes: number;
    reductionBytes: number;
    netBytes: number;
    limited: boolean;
  } | null;
};

export function AgentScanHistory({ locale }: { locale: "zh" | "en" }) {
  const tr = (zh: string, en: string) => locale === "zh" ? zh : en;
  const title = tr("MCP 扫描记录", "MCP scan history");
  const [open, setOpen] = useState(false);
  const [scopeId, setScopeId] = useState<string>();
  const [revision, setRevision] = useState(0);
  const [history, setHistory] = useState<AgentHistory>();
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState(false);
  const refresh = () => setRevision(previous => previous + 1);

  useEffect(() => {
    if (!open) return;
    let active = true;
    setLoading(true);
    setFailed(false);
    void diskInvoke<AgentHistory>("get_agent_scan_history", { scopeId }).then(result => {
      if (active) setHistory(result);
    }).catch(() => {
      if (active) setFailed(true);
    }).finally(() => {
      if (active) setLoading(false);
    });
    return () => { active = false; };
  }, [open, scopeId, revision]);

  const current = history?.scopes.find(scope => scope.id === history.selectedScopeId);
  const comparison = history?.comparison;
  const formatter = new Intl.DateTimeFormat(locale === "zh" ? "zh-CN" : "en-US", {
    dateStyle: "medium", timeStyle: "medium",
  });
  const date = (summary: ScanSummary) => formatter.format(summary.updatedAtMs);
  const scopeName = (scope: AgentScope) => scope.roots.map(root => root.split("/").filter(Boolean).at(-1) ?? root).join(" + ");
  const protection = (scope: AgentScope) => scope.allowProtected
    ? tr("含受保护目录", "Includes protected folders") : tr("跳过受保护目录", "Skips protected folders");

  return <Dialog open={open} onOpenChange={next => {
    if (next) {
      setHistory(undefined);
      setScopeId(undefined);
      setFailed(false);
      setLoading(true);
    }
    setOpen(next);
  }}>
    <DialogTrigger render={<Button variant="ghost" size="sm" className="comparison-mcp-trigger" />}>
      <History size={14} />{title}
    </DialogTrigger>
    <DialogContent className="comparison-mcp-dialog" closeLabel={tr("关闭", "Close")}>
      <DialogHeader><DialogTitle>{title}</DialogTitle></DialogHeader>
      <DialogBody className="comparison-mcp-body">
        {failed && <p role="alert">{current
          ? tr("读取失败，仍显示上次读取的记录。", "Could not refresh. Showing the last loaded history.")
          : tr("读取 MCP 扫描记录失败。", "Could not read MCP scan history.")}</p>}
        {loading && <p role="status"><LoaderCircle size={14} className="treemap-spinner" />{tr("正在读取扫描记录…", "Loading scan history…")}</p>}
        {!current && !loading && !failed && <p role="status">{tr("还没有已完成并保留的 MCP 扫描。清理回执可在「AI 连接」查看。", "No completed MCP scans have been saved. Cleanup receipts are available in AI connections.")}</p>}
        {history && current && <>
          {history.scopes.length > 1 && <NativeSelect aria-label={tr("MCP 扫描范围", "MCP scan scope")}
            value={current.id} disabled={loading} onValueChange={setScopeId}>
            {history.scopes.map(scope => <option key={scope.id} value={scope.id}>
              {scopeName(scope)} · {date(scope.current)} · {protection(scope)}
            </option>)}
          </NativeSelect>}
          <div className="comparison-mcp-scope">
            <strong>{current.roots.join(" · ")}</strong>
            <span>{protection(current)}</span>
          </div>
          <dl className="comparison-snapshot-dates">
            <div><dt>{tr("最近一次扫描", "Latest scan")}</dt><dd>{date(current.current)}</dd></div>
            <div><dt>{tr("已测占用", "Measured allocation")}</dt><dd>{formatBytes(current.allocatedBytes)}</dd></div>
            <div><dt>{tr("已扫描文件", "Files scanned")}</dt><dd>{current.scannedFiles.toLocaleString(locale)}</dd></div>
            {comparison && <div><dt>{tr("上一次可比扫描", "Previous comparable scan")}</dt><dd>{date(comparison.baseline)}</dd></div>}
          </dl>
          {!current.coverageComplete && <p>{tr("本次扫描仅覆盖部分内容。", "This scan covered only part of the scope.")}</p>}
          {!comparison
            ? <p>{tr("同一范围还没有可比的上一次扫描。", "No previous comparable scan for this scope.")}</p>
            : comparison.status === "ready"
              ? <>
                <dl className="comparison-metrics" aria-label={tr("MCP 占用变化", "MCP allocation changes")}>
                  <div><dt>{tr("新增占用", "Added usage")}</dt><dd>+{formatBytes(comparison.growthBytes)}</dd></div>
                  <div><dt>{tr("减少占用", "Removed usage")}</dt><dd>−{formatBytes(comparison.reductionBytes)}</dd></div>
                  <div><dt>{tr("占用净变化", "Net usage change")}</dt><dd>{comparison.netBytes < 0 ? "−" : comparison.netBytes > 0 ? "+" : ""}{formatBytes(Math.abs(comparison.netBytes))}</dd></div>
                </dl>
                {comparison.limited && <p>{tr("变化仅统计可比的已保存明细。", "Changes cover comparable saved details.")}</p>}
              </>
              : <p>{tr("两次扫描没有足够的可比明细。", "These scans have insufficient comparable detail.")}</p>}
        </>}
      </DialogBody>
      <DialogFooter><Button variant="outline" size="sm" onClick={refresh} disabled={loading}>
        {failed ? tr("重试", "Retry") : tr("刷新记录", "Refresh history")}
      </Button></DialogFooter>
    </DialogContent>
  </Dialog>;
}
