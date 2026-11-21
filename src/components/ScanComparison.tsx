import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { ArrowLeft, CircleHelp, History, LoaderCircle } from "lucide-react";
import { getDiskScanComparison, listDiskScanHistory, type DiskScan, type DiskScanComparison } from "../lib/disk-api";
import { comparisonTreemap, comparisonWithin } from "../lib/scan-comparison";
import { formatBytes } from "../lib/utils";
import { Button } from "./ui/button";
import { NativeSelect } from "./ui/native-select";
import { GROWTH_PREFERENCES_KEY, growthPreferenceFor, localDayKey, readGrowthPreferences, resolveGrowthBaseline, validateGrowthHistory, withGrowthPreference, type GrowthBaselineChoice, type GrowthHistory } from "../lib/growth-history";
import { Dialog, DialogBody, DialogContent, DialogHeader, DialogTitle } from "./ui/dialog";
import { DiskTreemap } from "./DiskTreemap";
import { SpaceSelection } from "./SpaceSelection";
import "../scan-comparison.css";

export type ScanComparisonProps = {
  scan: DiskScan | null;
  currentPath?: string;
  locale: "zh" | "en";
  height: number;
  query?: string;
  preview?: DiskScan | null;
  onBrowseCurrent: (path?: string) => void;
  onReveal: (path: string) => void;
  onTrashed: (path: string, resolvedPath?: string, result?: import("../lib/disk-api").EntryTrashResult) => void;
  onPermissionRequired: () => void;
};

export function ScanComparison({ scan, currentPath, locale, height, query = "", preview, onBrowseCurrent, onReveal, onTrashed, onPermissionRequired }: ScanComparisonProps) {
  const tr = (zh: string, en: string) => locale === "zh" ? zh : en;
  const sourceScope = JSON.stringify(scan?.sourceRoots ?? scan?.roots ?? []);
  const settledVersion = scan?.status === "running" ? 0 : scan?.updatedAtMs;
  const catalogKey = `${scan?.scanId ?? ""}\0${settledVersion ?? ""}\0${sourceScope}`;
  const [retry, setRetry] = useState(0);
  const [preferences, setPreferences] = useState(() => {
    try { return readGrowthPreferences(window.localStorage.getItem(GROWTH_PREFERENCES_KEY)); }
    catch { return readGrowthPreferences(null); }
  });
  const saveChoice = useCallback((scopeId: string, choice: GrowthBaselineChoice) => {
    setPreferences(previous => {
      if (JSON.stringify(growthPreferenceFor(previous, scopeId)) === JSON.stringify(choice)) return previous;
      const next = withGrowthPreference(previous, scopeId, choice);
      try { window.localStorage.setItem(GROWTH_PREFERENCES_KEY, JSON.stringify(next)); } catch { /* Keep this session usable when storage is unavailable. */ }
      return next;
    });
  }, []);
  const [catalog, setCatalog] = useState<{ key: string; sourceScope: string; data?: GrowthHistory; loading: boolean; error?: string }>({ key: catalogKey, sourceScope, loading: true });
  const catalogData = catalog.sourceScope === sourceScope ? catalog.data : undefined;
  const choice = catalogData ? growthPreferenceFor(preferences, catalogData.scopeId) : { mode: "day" } as const;
  const baseline = useMemo(() => catalogData ? resolveGrowthBaseline(catalogData, choice) : undefined, [catalogData, choice]);
  const catalogLoading = !!scan && (catalog.key !== catalogKey || catalog.loading);
  const catalogError = catalog.sourceScope === sourceScope ? catalog.error : undefined;
  const catalogReady = !!catalogData && catalog.key === catalogKey && !catalog.loading && !catalog.error;
  useEffect(() => {
    if (catalogData && baseline && JSON.stringify(choice) !== JSON.stringify(baseline.choice)) saveChoice(catalogData.scopeId, baseline.choice);
  }, [catalogData, baseline, choice, saveChoice]);
  useEffect(() => {
    if (!scan) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setCatalog(previous => ({ key: catalogKey, sourceScope, data: previous.sourceScope === sourceScope ? previous.data : undefined, loading: true }));
    const load = async () => {
      try {
        const next = validateGrowthHistory(await listDiskScanHistory(scan.scanId));
        if (!active) return;
        setCatalog({ key: catalogKey, sourceScope, data: next, loading: false });
        if (scan.status === "running") timer = setTimeout(load, 1200);
      } catch (cause) {
        if (!active) return;
        setCatalog(previous => ({ key: catalogKey, sourceScope, data: previous.sourceScope === sourceScope ? previous.data : undefined, loading: false, error: cause instanceof Error ? cause.message : String(cause) }));
      }
    };
    void load();
    return () => { active = false; clearTimeout(timer); };
  }, [scan?.scanId, scan?.status, settledVersion, catalogKey, sourceScope, retry]);

  // Historical drill belongs to the scope/path, not an individual rescan ID.
  const origin = `${catalogData?.scopeId ?? sourceScope}\0${currentPath ?? ""}`;
  const [navigation, setNavigation] = useState<{ origin: string; path?: string; history: (string | undefined)[] }>({ origin, path: currentPath, history: [] });
  const path = navigation.origin === origin ? navigation.path : currentPath;
  const history = navigation.origin === origin ? navigation.history : [];
  const identity = JSON.stringify([catalogData?.scopeId, baseline?.baseline?.id, path]);
  const key = JSON.stringify([scan?.scanId, baseline?.current?.id, identity]);
  const [infoOpen, setInfoOpen] = useState(false);
  const stageRef = useRef<HTMLDivElement>(null);
  const locationRef = useRef<HTMLSpanElement>(null);
  const focusLocation = useRef(false);
  const [stageHeight, setStageHeight] = useState<number>();
  const [panelSide, setPanelSide] = useState<"left" | "right">("right");
  const [selection, setSelection] = useState<{ key: string; id: string }>();
  const [result, setResult] = useState<{ key: string; identity: string; data?: DiskScanComparison; loading: boolean; error?: string }>({ key, identity, loading: true });
  const data = baseline?.status === "ready" && result.identity === identity ? result.data : undefined;
  const loading = catalogLoading || (!!scan && baseline?.status === "ready" && (result.key !== key || result.loading));
  const error = catalogError ?? (result.identity === identity ? result.error : undefined);
  const baselineUnavailable = baseline?.status === "baseline-unavailable" || data?.status === "baseline-unavailable";

  useEffect(() => {
    if (!scan || !catalogReady || baseline?.status !== "ready" || !baseline.baseline || !baseline.current) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const baselineId = baseline.baseline.id;
    const currentId = baseline.current.id;
    setResult(previous => ({ key, identity, data: previous.identity === identity ? previous.data : undefined, loading: true }));
    const load = async () => {
      try {
        const next = await getDiskScanComparison(scan.scanId, path, baselineId);
        if (!active) return;
        if (next.currentId && next.currentId !== currentId) { setRetry(value => value + 1); return; }
        if (next.baselineId && next.baselineId !== baselineId) throw new Error("The selected baseline could not be loaded");
        setResult({ key, identity, data: next, loading: false });
        if (next.status === "scanning" || scan.status === "running" || next.refreshing) timer = setTimeout(load, 1200);
      } catch (cause) {
        if (!active) return;
        setResult(previous => ({ key, identity, data: previous.identity === identity ? previous.data : undefined, loading: false, error: cause instanceof Error ? cause.message : String(cause) }));
      }
    };
    void load();
    return () => { active = false; clearTimeout(timer); };
  }, [scan?.scanId, scan?.status, settledVersion, key, identity, path, retry, catalogReady, baseline?.status, baseline?.baseline?.id, baseline?.current?.id]);

  const ready = data?.status === "ready";
  const scanning = scan?.status === "running" || data?.status === "scanning" || !!data?.refreshing;
  const model = useMemo(() => data?.status === "ready"
    ? comparisonTreemap(data, "growth", path, tr("其他增长", "Other growth")) : undefined, [data, path, locale]);
  const selectedId = selection?.key === identity ? selection.id : undefined;
  const selected = selectedId ? model?.details.get(selectedId) : undefined;
  const progress = scan ? `${scan.scannedFiles.toLocaleString()} ${tr("项", "items")} · ${Math.floor(scan.elapsedMs / 1000)}s` : "";
  const showPreview = !ready && !baselineUnavailable && !error && scanning && path === currentPath && !!preview?.entries.some(entry => entry.allocatedBytes > 0);
  const navigate = (next?: string) => {
    if (next === path) return;
    focusLocation.current = true;
    setNavigation({ origin, path: next, history: [...history, path] });
    setSelection(undefined);
  };
  const openDirectory = (next: string) => {
    // A residual represents this directory, not a fictitious child directory.
    // Its deeper historical contents cannot be reconstructed from a live scan.
    if (next === path) onBrowseCurrent(next);
    else navigate(next);
  };
  const back = () => {
    if (!history.length) return;
    focusLocation.current = true;
    setNavigation({ origin, path: history[history.length - 1], history: history.slice(0, -1) });
    setSelection(undefined);
  };
  useEffect(() => {
    if (!ready || !focusLocation.current) return;
    locationRef.current?.focus();
    focusLocation.current = false;
  }, [key, ready]);
  const keyboard = (event: KeyboardEvent<HTMLElement>) => {
    const target = event.target as HTMLElement;
    if (event.defaultPrevented || target.closest("input, textarea, [contenteditable=true], [role=dialog]")) return;
    if ((event.altKey && event.key === "ArrowLeft") || (event.metaKey && event.key === "[")) {
      event.preventDefault(); back();
    }
    if (event.key === "Escape") setSelection(undefined);
  };
  const roots = data?.roots ?? scan?.roots ?? [];
  const owningRoot = path ? roots.filter(root => comparisonWithin(path, root)).sort((a, b) => b.length - a.length)[0] : undefined;
  const crumbs: { path: string; name: string }[] = [];
  if (path && owningRoot) {
    crumbs.push({ path: owningRoot, name: owningRoot.split("/").pop() || "/" });
    const suffix = path.slice(owningRoot === "/" ? 1 : owningRoot.length + 1);
    let cursor = owningRoot;
    if (path !== owningRoot) for (const name of suffix.split("/").filter(Boolean)) {
      cursor = `${cursor === "/" ? "" : cursor}/${name}`;
      crumbs.push({ path: cursor, name });
    }
  } else if (path) crumbs.push({ path, name: path });
  const date = (timestamp?: number) => timestamp && Number.isFinite(timestamp)
    ? new Date(timestamp).toLocaleString(locale === "zh" ? "zh-CN" : "en-US", { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })
    : tr("时间未知", "Time unavailable");
  const snapshotLabel = (timestamp: number) => new Date(timestamp).toLocaleString(locale === "zh" ? "zh-CN" : "en-US", { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit", fractionalSecondDigits: 3 });
  const compactDate = (timestamp?: number) => timestamp ? new Date(timestamp).toLocaleString(locale === "zh" ? "zh-CN" : "en-US", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : tr("时间未知", "Time unavailable");
  const rangeStart = ready ? data.baselineUpdatedAtMs : baseline?.baseline?.updatedAtMs;
  const rangeEnd = ready ? data.currentUpdatedAtMs : baseline?.current?.updatedAtMs;
  const range = rangeStart && rangeEnd ? `${baseline?.firstOfDay ? localDayKey(rangeStart) === localDayKey(Date.now()) ? tr("自今日最早记录 ", "Since earliest saved today ") : tr("自当日最早记录 ", "Since earliest saved that day ") : ""}${compactDate(rangeStart)}${baseline?.firstOfDay ? tr(" 起", "") : ""} → ${compactDate(rangeEnd)}` : undefined;
  const selectValue = baselineUnavailable ? "unavailable" : choice.mode === "snapshot" ? `snapshot:${choice.id}` : choice.mode;
  const changeBaseline = (value: string) => {
    if (!catalogData) return;
    if (value === "day" || value === "previous") saveChoice(catalogData.scopeId, { mode: value });
    else if (value.startsWith("snapshot:")) {
      const selected = catalogData.snapshots.find(item => item.id === value.slice(9));
      if (selected) saveChoice(catalogData.scopeId, { mode: "snapshot", id: selected.id, updatedAtMs: selected.updatedAtMs });
    }
    setSelection(undefined);
  };
  const represented = model?.entries.filter(entry => model.roots.includes(entry.path)).reduce((sum, entry) => sum + entry.allocatedBytes, 0) ?? 0;
  const unlocated = Math.max(0, (ready ? data.growthBytes : 0) - represented);
  const historyEndsHere = !!model && model.entries.filter(entry => entry.allocatedBytes > 0 && !model.roots.includes(entry.path))
    .every(entry => { const detail = model.details.get(entry.path); return detail?.residual && detail.path === path; }) && represented > 0;
  const mapHeight = Math.max(240, stageHeight ?? Math.min(440, height - 36));
  useLayoutEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const update = () => setStageHeight(Math.floor(stage.clientHeight));
    const observer = new ResizeObserver(update);
    observer.observe(stage);
    update();
    return () => observer.disconnect();
  }, [ready, key]);

  const emptyTitle = !scan ? tr("先完成一次扫描", "Complete a scan first")
    : loading ? tr("正在读取增长…", "Loading growth…")
    : error ? tr("暂时无法读取增长", "Growth could not be loaded")
    : baselineUnavailable ? tr("所选基准已不可用，请重新选择", "Selected baseline unavailable; choose another")
    : baseline?.status === "no-current" ? scanning ? tr("等待完整扫描记录", "Waiting for a complete scan") : tr("最新完整记录不可用，请重新扫描", "Latest complete record unavailable; rescan this scope")
    : baseline?.status === "awaiting-next" ? tr("已固定最早记录，等待下一次完整扫描", "Earliest saved record pinned; complete another scan to compare")
    : baseline?.status === "no-baseline" || data?.status === "no-baseline" ? tr("还没有可对比的上次扫描", "No previous scan to compare")
    : data?.status === "scanning" ? tr("扫描完成后查看增长", "Growth is available when the scan completes")
    : !ready ? tr("暂无可比较的记录", "No comparable records")
    : unlocated > 0 ? tr("增长明细暂不可用", "Growth details unavailable")
    : data.limited ? tr("已记录范围内没有新增占用", "No added usage in retained records") : tr("没有新增占用", "No added usage");

  return <section className="scan-comparison" aria-label={tr("新增占用", "Changes in this scan")} onKeyDown={keyboard}>
    <div className="comparison-baseline-row">
      <NativeSelect aria-label={tr("增长基准", "Growth baseline")} className="comparison-baseline-select" value={selectValue} onValueChange={changeBaseline} disabled={!catalogReady}>
        {baselineUnavailable && <option value="unavailable" disabled>{tr("基准不可用 · 请重选", "Baseline unavailable · choose again")}</option>}
        <option value="day">{baseline?.current && localDayKey(baseline.current.updatedAtMs) !== localDayKey(Date.now()) ? tr("扫描当日开始", "Start of scan day") : tr("今天开始", "Start of today")}</option>
        <option value="previous">{tr("上次扫描", "Previous scan")}</option>
        {!!catalogData?.snapshots.length && <optgroup label={tr("已保存扫描", "Saved scans")}>
          {catalogData.snapshots.map(item => <option key={item.id} value={`snapshot:${item.id}`}>{snapshotLabel(item.updatedAtMs)}</option>)}
        </optgroup>}
      </NativeSelect>
      {range && <span className="comparison-range" title={`${date(rangeStart)} → ${date(rangeEnd)}`}>{range}</span>}
    </div>
    {ready && <dl className="comparison-metrics" aria-label={tr("已比对范围的占用变化", "Allocation changes in compared contents")}>
      <div><dt>{tr("新增占用", "Added usage")}</dt><dd className="comparison-total"><b>+{formatBytes(data.growthBytes)}</b></dd></div>
      <div><dt>{tr("减少占用", "Removed usage")}</dt><dd>−{formatBytes(data.reductionBytes)}</dd></div>
      <div><dt>{data.netBytes < 0 ? tr("占用净减少", "Net usage decrease") : data.netBytes > 0 ? tr("占用净增加", "Net usage increase") : tr("占用净变化", "Net usage change")}</dt><dd>{formatBytes(Math.abs(data.netBytes))}</dd></div>
    </dl>}
    <div className="comparison-location directory-location">
      {!!history.length && <Button size="icon-sm" variant="ghost" onClick={back} aria-label={tr("返回上一个历史范围", "Back to previous comparison scope")}><ArrowLeft size={14} /></Button>}
      <nav aria-label={tr("历史对比路径", "Comparison path")}><ol>
        <li>{!path ? <span ref={locationRef} tabIndex={-1} aria-current="location">{tr("扫描范围", "Scan scope")}</span> : <button type="button" onClick={() => navigate(undefined)}>{tr("扫描范围", "Scan scope")}</button>}</li>
        {crumbs.map(crumb => <li key={crumb.path}>{crumb.path === path ? <span ref={locationRef} tabIndex={-1} title={crumb.path} aria-current="location">{crumb.name}</span> : <button type="button" title={crumb.path} onClick={() => navigate(crumb.path)}>{crumb.name}</button>}</li>)}
      </ol></nav>
      {!ready && scanning && <div className="comparison-summary"><LoaderCircle size={12} className="treemap-spinner" /><span>{progress}</span><Button size="sm" variant="ghost" onClick={() => onBrowseCurrent(path)}>{tr("查看当前占用", "Browse current usage")}</Button></div>}
      {ready && <div className="comparison-summary">
        {(loading || scanning) && <LoaderCircle size={12} className="treemap-spinner" aria-label={scanning ? tr("扫描中，显示已完成对比", "Scanning; showing completed comparison") : tr("正在更新", "Updating")} />}
        {scanning && <span className="comparison-partial" title={progress}>{tr("更新中", "Updating")}</span>}
        {(data.limited || unlocated > 0) && <span className="comparison-partial">{tr("已比对部分", "Compared portion")}</span>}
        <Button variant="ghost" size="icon-sm" className="comparison-info" onClick={() => setInfoOpen(true)} aria-label={tr("对比详情", "Comparison details")}><CircleHelp size={13} /></Button>
      </div>}
    </div>
    <div ref={stageRef} className="comparison-stage">
      {showPreview && <div className="comparison-preview-label">{tr("当前占用预览 · 增长对比更新中", "Current usage preview · updating growth")}</div>}
      {ready && represented > 0 && model ? <DiskTreemap
        entries={model.entries} roots={model.roots} locale={locale} height={mapHeight}
        isScanning={false} minimal metric="growth" query={query} selectedPath={selectedId} onClearSelection={() => setSelection(undefined)}
        entryPathLabel={entry => model.details.get(entry.path)?.path ?? entry.path}
        entrySummary={entry => {
          const detail = model.details.get(entry.path);
          return detail ? `${tr("已比对部分", "Compared contents")} ${formatBytes(detail.beforeBytes)} → ${formatBytes(detail.afterBytes)}${detail.residual && detail.path === path ? tr(" · 此部分为目录汇总，按 Enter 查看当前内容", " · Directory aggregate; press Enter to browse current contents") : ""}` : "";
        }}
        onSelect={(entry, side) => { setPanelSide(side ?? "right"); setSelection({ key: identity, id: entry.path }); }}
        onOpenDirectory={entry => openDirectory(model.details.get(entry.path)?.path ?? entry.path)}
        onRevealFile={entry => onReveal(model.details.get(entry.path)?.path ?? entry.path)}
        onInspectDirectory={directory => openDirectory(model.details.get(directory)?.path ?? directory)}
      /> : showPreview && preview ? <DiskTreemap entries={preview.entries} roots={preview.roots} locale={locale} height={Math.max(240, mapHeight - 24)} isScanning={true} minimal query={query}
        onOpenDirectory={entry => onBrowseCurrent(entry.path)} onRevealFile={entry => onReveal(entry.path)} onInspectDirectory={onBrowseCurrent} />
      : <div className="comparison-empty" role="status">
        {scan && (loading || data?.status === "scanning") ? <LoaderCircle size={23} className="treemap-spinner" /> : <History size={23} />}
        <strong>{emptyTitle}</strong>
        {(baseline?.status === "no-baseline" || data?.status === "no-baseline") && <p>{tr("同一范围再扫描一次即可对比。", "Scan the same scope again to compare.")}</p>}
        {scanning && !showPreview && <div className="comparison-scan-paths">{(preview?.entries.filter(entry => entry.kind !== "file").slice(0, 6) ?? []).map(entry => <Button key={entry.path} variant="outline" onClick={() => onBrowseCurrent(entry.path)}>{entry.name}</Button>)}</div>}
        {error && <Button size="sm" variant="ghost" onClick={() => setRetry(value => value + 1)}>{tr("重试", "Retry")}</Button>}
      </div>}
      {selected && <SpaceSelection name={selected.residual ? selected.path.split("/").pop() || selected.name : selected.name} path={selected.path} locale={locale} side={panelSide}
        historical
        metrics={[{label: tr("增长", "Growth"), value:`+${formatBytes(selected.growthBytes)}`}]}
        partial={selected.detailLimited} onClose={() => setSelection(undefined)}
        openLabel={selected.path === path ? tr("查看当前内容", "Browse current contents") : undefined}
        onOpen={selected.kind === "directory" ? () => openDirectory(selected.path) : undefined}
        onReveal={() => onReveal(selected.path)} onTrashed={(path, resolvedPath, result) => { setSelection(undefined); onTrashed(path, resolvedPath, result); }} onPermissionRequired={onPermissionRequired} />}
    </div>
    {ready && historyEndsHere && <div className="comparison-history-limit" role="status"><span>{tr("此部分为目录汇总，无法继续细分", "Directory aggregate; no finer historical breakdown")}</span><Button size="sm" variant="ghost" onClick={() => onBrowseCurrent(path)}>{tr("查看当前内容", "Browse current contents")}</Button></div>}
    {ready && error && <div className="comparison-error" role="alert">{tr("更新失败，显示上次对比", "Update failed; showing the previous comparison")}<Button size="sm" variant="ghost" onClick={() => setRetry(value => value + 1)}>{tr("重试", "Retry")}</Button></div>}
    <Dialog open={infoOpen && ready} onOpenChange={setInfoOpen}>
      <DialogContent className="comparison-info-dialog">
        <DialogHeader><DialogTitle>{tr("对比详情", "Comparison details")}</DialogTitle></DialogHeader>
        <DialogBody>
          <dl className="comparison-snapshot-dates"><div><dt>{tr("起点扫描", "Baseline scan")}</dt><dd>{date(data?.baselineUpdatedAtMs)}</dd></div><div><dt>{tr("最新完整扫描", "Latest complete scan")}</dt><dd>{date(data?.currentUpdatedAtMs)}</dd></div></dl>
          <p>{tr("比较选定起点与最新完整终点，只显示增加部分，不累加中间各次扫描的正增长。新扫描完成前保留完整旧对比。", "Compares the selected baseline with the latest complete endpoint and shows increases only; it does not sum positive changes between intermediate scans. The complete comparison stays visible while scanning.")}</p>
          <p>{tr("方块图只展示新增占用；减少占用单独统计，两者相减得到占用净变化。即使占用净减少，也可能有新文件或已有文件变大。这些数值属于已比对的扫描范围，不等于整块磁盘可用空间的变化。", "The map shows added usage. Removed usage is counted separately; their difference is the net usage change. New files or growing files can remain visible even when net usage falls. These measurements describe compared scan contents, not the change in free space across the entire disk.")}</p>
          {baseline?.firstOfDay && <p>{tr("当天没有更早的完整记录，实际起点为该日现存最早记录。更早历史可能未保存，因此不代表全天增长。", "The baseline is this day's earliest retained complete record. Earlier history may not have been saved, so this is not a full-day measurement.")}</p>}
          <p>{tr("按已保存的目录明细计算增长，未细分内容内部的变化可能相抵。悬停方块可查看已比对部分。", "Growth is compared at the retained level of directory detail; changes within unitemized contents may offset. Hover a tile to inspect compared contents.")}</p>
          <p>{tr("移动、重命名或硬链接归属变化也可能呈现为增长，不等于磁盘物理占用变化。", "Moves, renames and hard-link attribution can appear as growth without a physical disk change.")}</p>
          {data?.limited && <p>{tr("部分明细未保留或无法读取。", "Some details were not retained or could not be read.")}</p>}
          {unlocated > 0 && <p>{tr(`图中 ${formatBytes(represented)}，另有 ${formatBytes(unlocated)} 未逐项显示。`, `Map shows ${formatBytes(represented)}; ${formatBytes(unlocated)} is not itemized.`)}</p>}
        </DialogBody>
      </DialogContent>
    </Dialog>
  </section>;
}
