import { useEffect, useRef, useState } from "react";
import { ArrowUpRight, File, Folder, LoaderCircle, RefreshCw } from "lucide-react";
import { Button } from "./ui/button";
import { listDiskDirectory, type DiskDirectoryPage, type DiskEntry, type DiskScan } from "../lib/disk-api";
import { formatBytes } from "../lib/utils";
import "../directory-contents.css";

// Only a handful of displayed pages live in memory. No file inventory is
// persisted; navigating back reuses a page briefly without growing the index.
const pages = new Map<string, { at: number; value: DiskDirectoryPage; bytes: number }>();
export function DirectoryContents({ source, summaries, path, query, locale, onOpen, onReveal }: {
  source: DiskScan; summaries?: DiskEntry[]; path: string; query: string; locale: "zh" | "en";
  onOpen: (path: string) => void; onReveal: (path: string) => void;
}) {
  const tr = (zh: string, en: string) => locale === "zh" ? zh : en;
  const [result, setResult] = useState<DiskDirectoryPage>();
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const [page, setPage] = useState(0);
  const request = useRef(0);
  const context = `${source.scanId}:${path}`;
  const contextRef = useRef(context);
  contextRef.current = context;
  const load = async (cursor?: string, force = false) => {
    const round = ++request.current;
    const key = `${context}:${source.updatedAtMs}:${cursor ?? ""}`;
    setLoading(true); setError("");
    try {
      const cached = pages.get(key);
      const next = !force && cached && Date.now() - cached.at < 30_000
        ? cached.value : await listDiskDirectory(source.scanId, path, cursor);
      if (round !== request.current || contextRef.current !== context) return false;
      pages.delete(key);
      const bytes = JSON.stringify(next).length * 2 + key.length * 2;
      if (bytes <= 2 * 1024 * 1024) pages.set(key, { at: Date.now(), value: next, bytes });
      while (pages.size > 8 || [...pages.values()].reduce((sum, item) => sum + item.bytes, 0) > 2 * 1024 * 1024)
        pages.delete(pages.keys().next().value!);
      setResult(next);
      return true;
    } catch (e) {
      if (round === request.current && contextRef.current === context) setError(String(e));
      return false;
    } finally {
      if (round === request.current && contextRef.current === context) setLoading(false);
    }
  };
  const changePage = async (nextPage: number, cursor?: string) => {
    if (await load(cursor)) {
      setPage(nextPage);
      setCursors(previous => [...previous.slice(0, nextPage), cursor]);
    }
  };
  useEffect(() => {
    setResult(undefined); setPage(0); setCursors([undefined]);
    void load();
    return () => { ++request.current; };
  }, [context, source.status]);
  const measured = new Map(summaries?.filter(item => item.parentPath === path && item.kind !== "file")
    .map(item => [item.path, item]));
  const visible = (result?.path === path ? result.entries : []).map(item => {
    if (item.kind !== "directory" && item.kind !== "application") return item;
    const summary = measured.get(item.path);
    if (!summary || (!summary.complete && !summary.directoryCount)) return item;
    return { ...item, allocatedBytes: summary.allocatedBytes, complete: summary.complete, measurement: "summary" as const };
  }).filter(item =>
    `${item.name} ${item.path}`.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase()),
  ).sort((a, b) => (b.allocatedBytes ?? -1) - (a.allocatedBytes ?? -1) || a.name.localeCompare(b.name));
  return <section className="directory-contents" aria-label={tr("目录明细", "Directory contents")}>
    <div className="directory-toolbar">
      <span>{tr("本层文件与文件夹", "Files and folders")}</span>
      <small>{tr("目录：扫描统计 · 文件：实时元数据", "Folders: scan results · files: live metadata")}</small>
      <div />
      <Button variant="ghost" size="icon-sm" aria-label={tr("刷新本层明细", "Refresh directory listing")} disabled={loading}
        onClick={() => void load(cursors[page], true)}><RefreshCw size={14} className={loading ? "spin" : ""} /></Button>
    </div>
    {error && <div className="error-banner" role="alert">{error}<Button variant="outline" onClick={() => void load(cursors[page], true)}>{tr("重试", "Retry")}</Button></div>}
    <div className="directory-scroll" aria-busy={loading}>
      <div className="directory-row directory-head"><span>{tr("名称", "Name")}</span><span>{tr("实际占用", "Allocated")}</span><span /></div>
      {visible.map(item => <div className="directory-row" key={item.path}>
        <button className="directory-name" title={item.path} disabled={item.measurement === "excluded"}
          onClick={() => item.kind === "directory" || item.kind === "application" ? onOpen(item.path) : onReveal(item.path)}>
          {item.kind === "directory" || item.kind === "application" ? <Folder size={16} /> : <File size={16} />}
          <span>{item.name}</span>
        </button>
        <span title={item.hardLinkIdentity ? tr("文件自身占用；硬链接可能与其他文件共享，不能直接相加", "File allocation; hard links may share storage and cannot simply be added") : undefined}>
          {item.allocatedBytes !== undefined ? <>{!item.complete && "≥ "}{formatBytes(item.allocatedBytes)}{item.measurement === "summary" && <small className="directory-measurement">{item.complete ? tr("已统计", "Measured") : tr("部分统计", "Partial")}</small>}</> : item.measurement === "excluded" ? tr("已跳过", "Skipped") : item.kind === "directory" || item.kind === "application" ? <Button variant="ghost" onClick={() => onOpen(item.path)}>{tr("打开目录", "Open folder")}</Button> : tr("无法读取", "Unreadable")}
        </span>
        <Button variant="ghost" size="icon-sm" aria-label={`${tr("在 Finder 显示", "Reveal")} ${item.name}`} onClick={() => onReveal(item.path)}><ArrowUpRight size={13} /></Button>
      </div>)}
      {!visible.length && <div className="disk-empty" role="status">{loading ? <><LoaderCircle size={15} className="spin" />{tr("读取本层明细…", "Reading this directory…")}</> : query ? tr("本页没有匹配项", "No matches on this page") : error ? "" : tr("此目录为空", "This directory is empty")}</div>}
    </div>
    <div className="directory-footer">
      <span>{loading ? tr("读取中…", "Loading…") : tr(`第 ${page + 1} 页 · ${result?.entries.length ?? 0} 项`, `Page ${page + 1} · ${result?.entries.length ?? 0} items`)}{query && tr(" · 搜索本页", " · searching this page")}</span>
      <Button variant="ghost" disabled={!page || loading} onClick={() => void changePage(page - 1, cursors[page - 1])}>{tr("上一页", "Previous")}</Button>
      <Button variant="ghost" disabled={!result?.hasMore || loading} onClick={() => void changePage(page + 1, result?.nextAfterPath)}>{tr("下一页", "Next")}</Button>
    </div>
    {!!result?.issues.length && <details className="directory-issues"><summary>{tr(`${result.issues.length} 项无法读取`, `${result.issues.length} unreadable items`)}</summary>{result.issues.map((issue, i) => <p key={i}>{issue.path}: {issue.message}</p>)}</details>}
  </section>;
}
