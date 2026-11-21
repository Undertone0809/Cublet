import {
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type FocusEvent,
  type KeyboardEvent,
  type PointerEvent,
} from "react";
import {
  ArrowUpRight,
  File,
  Folder,
  Layers,
  LoaderCircle,
  MousePointer2,
} from "lucide-react";
import { formatBytes } from "../lib/utils";
import { Button } from "./ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogBody } from "./ui/dialog";
import type { DiskEntry } from "../lib/disk-api";
import {
  buildTreemapTree,
  layoutTreemap,
  treemapColorIndex,
  type TreemapTile,
  type TreemapNode,
} from "../lib/treemap-layout";
import "../treemap.css";

export type DiskTreemapProps = {
  entries: DiskEntry[];
  roots: string[];
  onOpenDirectory: (entry: DiskEntry) => void;
  onRevealFile: (entry: DiskEntry) => void;
  onInspectDirectory: (path: string) => void;
  onClearSelection?: () => void;
  onSelect?: (entry: DiskEntry, panelSide?: "left" | "right") => void;
  locale: "zh" | "en";
  isScanning: boolean;
  selectedPath?: string;
  height?: number;
  query?: string;
  minimal?: boolean;
  metric?: "allocation" | "growth" | "reduction";
  entryPathLabel?: (entry: DiskEntry) => string;
  entrySummary?: (entry: DiskEntry) => string;
};

const colors = [
  "#92633b",
  "#327665",
  "#526696",
  "#805d88",
  "#697139",
  "#426a8c",
  "#995950",
  "#4c7356",
];
const sizeLabel = (value: number, _locale: "zh" | "en") => formatBytes(value);

const tooltipInset = 8;
const tooltipGap = 10;
const coverageHeight = 22;

function fitTooltipAxis(start: number, end: number, size: number, limit: number) {
  const after = end + tooltipGap;
  if (after + size <= limit - tooltipInset) return after;
  const before = start - tooltipGap - size;
  if (before >= tooltipInset) return before;
}

function clampTooltipAxis(anchor: number, size: number, limit: number) {
  return Math.max(tooltipInset, Math.min(anchor + tooltipGap, limit - size - tooltipInset));
}


export function DiskTreemap({
  entries,
  roots,
  onOpenDirectory,
  onRevealFile,
  onInspectDirectory,
  onSelect,
  onClearSelection,
  locale,
  isScanning,
  selectedPath,
  height = 440,
  query = "",
  minimal = false,
  metric = "allocation",
  entryPathLabel = (entry) => entry.path,
  entrySummary,
}: DiskTreemapProps) {
  const tr = (zh: string, en: string) => (locale === "zh" ? zh : en);
  const isChange = metric !== "allocation";
  const metricLabel = metric === "growth" ? tr("增长", "growth") : metric === "reduction" ? tr("减少", "reduction") : tr("占用", "allocation");
  const chartRef = useRef<HTMLDivElement>(null);
  const tooltipRef = useRef<HTMLDivElement>(null);
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  const [width, setWidth] = useState(0);
  const [localSelection, setLocalSelection] = useState<string>();
  const [focusedPath, setFocusedPath] = useState<string>();
  const [group, setGroup] = useState<TreemapNode[]>();
  const [groupPage, setGroupPage] = useState(0);
  const [hover, setHover] = useState<{
    tile: TreemapTile;
    pointer?: { x: number; y: number };
  }>();
  const descriptionId = useId();
  const coverageId = useId();
  const { nodes: forest, mappedBytes, unmappedBytes } = useMemo(
    () => buildTreemapTree(entries, roots),
    [entries, roots],
  );
  const total = mappedBytes + unmappedBytes;
  const showCoverage = unmappedBytes > 0;
  const chartHeight = Math.max(240, height) - (minimal && showCoverage ? coverageHeight : 0);
  const provisional = forest.some((node) => node.provisional);
  const partial =
    provisional ||
    forest.some((node) => node.entry && !node.entry.complete) ||
    entries.some((entry) => roots.includes(entry.path) && !entry.complete);
  const tiles = useMemo(
    () => layoutTreemap(forest, { x: 0, y: 0, width, height: chartHeight }, undefined, {
      reserveForDescendants: !isChange,
    }),
    [forest, width, chartHeight, isChange],
  );
  const chosenPath = onSelect ? selectedPath : localSelection;
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const realTiles = tiles.filter((tile) => tile.node.entry || tile.node.inspectPath || tile.node.groupedNodes);
  const tabPath = realTiles.some((tile) => tile.node.id === focusedPath)
    ? focusedPath
    : realTiles.some((tile) => tile.node.id === chosenPath)
      ? chosenPath
      : realTiles[0]?.node.id;
  const selected = entries.find((entry) => entry.path === chosenPath);
  const legend = forest.slice(0, 6);
  const percent = (value: number) =>
    `${(mappedBytes > 0 ? (value / mappedBytes) * 100 : 0).toLocaleString(locale === "zh" ? "zh-CN" : "en-US", { maximumFractionDigits: 1 })}%`;
  const nodeName = (node: TreemapNode) => node.synthetic === "contents"
    ? tr("其他内容", "Other contents")
    : node.synthetic === "group"
      ? tr(`${node.groupedCount} 项合并`, `${node.groupedCount} grouped items`)
      : node.name;
  const syntheticAction = (node: TreemapNode) => node.synthetic === "group"
    ? tr(`展开 ${node.groupedCount} 项`, `Show ${node.groupedCount} items`)
    : tr("打开所属文件夹", "Open containing folder");
  const activate = (entry: DiskEntry) =>
    entry.kind === "file" ? onRevealFile(entry) : onOpenDirectory(entry);
  const select = (entry: DiskEntry, tile: { x: number; width: number }) => {
    setLocalSelection(entry.path);
    onSelect?.(entry, tile.x + tile.width / 2 > width / 2 ? "left" : "right");
  };
  const expand = (node: TreemapNode) => {
    if (node.groupedNodes) { setGroup(node.groupedNodes); setGroupPage(0); }
    else if (node.inspectPath) onInspectDirectory(node.inspectPath);
  };

  useEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    const observer = new ResizeObserver(([entry]) =>
      setWidth(entry.contentRect.width),
    );
    observer.observe(chart);
    setWidth(chart.clientWidth);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    setHover(undefined);
  }, [entries, roots, width, chartHeight]);
  useLayoutEffect(() => {
    const chart = chartRef.current;
    const tooltip = tooltipRef.current;
    const hit = hover && buttons.current.get(hover.tile.node.id);
    if (!chart || !tooltip || !hit) return;

    const chartBox = chart.getBoundingClientRect();
    const hitBox = hit.getBoundingClientRect();
    const originX = chartBox.left + chart.clientLeft;
    const originY = chartBox.top + chart.clientTop;
    const x = (hover.pointer?.x ?? hitBox.left + hitBox.width / 2) - originX;
    const y = (hover.pointer?.y ?? hitBox.top + hitBox.height / 2) - originY;
    const { offsetWidth: cardWidth, offsetHeight: cardHeight } = tooltip;
    const { clientWidth, clientHeight } = chart;

    let left = fitTooltipAxis(hitBox.left - originX, hitBox.right - originX, cardWidth, clientWidth);
    let top = fitTooltipAxis(hitBox.top - originY, hitBox.bottom - originY, cardHeight, clientHeight);

    // On large tiles, fall back to keeping the pointer clear if the whole tile cannot be avoided.
    if (left === undefined && top === undefined) {
      left = fitTooltipAxis(x, x, cardWidth, clientWidth);
      top = fitTooltipAxis(y, y, cardHeight, clientHeight);
    }
    if (left === undefined && top === undefined) {
      tooltip.style.opacity = "0";
      return;
    }
    left ??= clampTooltipAxis(x, cardWidth, clientWidth);
    top ??= clampTooltipAxis(y, cardHeight, clientHeight);
    tooltip.style.translate = `${left}px ${top}px`;
    tooltip.style.opacity = "1";
  }, [hover, width, chartHeight, locale]);

  const moveFocus = (
    event: KeyboardEvent<HTMLButtonElement>,
    tile: TreemapTile,
  ) => {
    if (event.altKey || event.metaKey || event.ctrlKey) return;
    if (event.key === "Enter") {
      event.preventDefault();
      if (tile.node.entry) activate(tile.node.entry);
      else expand(tile.node);
      return;
    }
    if (
      ![
        "ArrowLeft",
        "ArrowRight",
        "ArrowUp",
        "ArrowDown",
        "Home",
        "End",
        "Escape",
      ].includes(event.key)
    )
      return;
    event.preventDefault();
    if (event.key === "Escape") {
      onClearSelection?.();
      setLocalSelection(undefined);
      setHover(undefined);
      return;
    }
    let target: TreemapTile | undefined;
    if (event.key === "Home" || event.key === "End")
      target =
        event.key === "Home" ? realTiles[0] : realTiles[realTiles.length - 1];
    else {
      const x = tile.x + tile.width / 2;
      const y = tile.y + (tile.hasChildren ? 12 : tile.height / 2);
      let closest = Infinity;
      for (const candidate of realTiles) {
        if (candidate.node.id === tile.node.id) continue;
        const dx = candidate.x + candidate.width / 2 - x;
        const dy =
          candidate.y + (candidate.hasChildren ? 12 : candidate.height / 2) - y;
        const horizontal =
          event.key === "ArrowLeft" || event.key === "ArrowRight";
        const along =
          event.key === "ArrowLeft"
            ? -dx
            : event.key === "ArrowRight"
              ? dx
              : event.key === "ArrowUp"
                ? -dy
                : dy;
        if (along <= 0) continue;
        const across = Math.abs(horizontal ? dy : dx);
        const score = along + across * 2 + Math.hypot(dx, dy) * 0.1;
        if (score < closest) {
          closest = score;
          target = candidate;
        }
      }
    }
    if (target) {
      setFocusedPath(target.node.id);
      buttons.current.get(target.node.id)?.focus();
    }
  };

  return (
    <section
      className={`disk-treemap ${minimal ? "is-minimal" : ""}`}
      aria-label={isChange ? `${metricLabel}${tr("树图", " treemap")}` : tr("磁盘空间矩形树图", "Disk space treemap")}
    >
      <div className="treemap-heading">
        <div className="treemap-heading-label">
          <Layers size={15} />
          <strong>{isChange ? `${metricLabel}${tr("地图", " map")}` : tr("空间地图", "Space map")}</strong>
          {isScanning && (
            <span className="treemap-live">
              <LoaderCircle size={11} className="treemap-spinner" />
              {tr("实时更新", "Live")}
            </span>
          )}
          {!isScanning && partial && (
            <span className="treemap-live">{tr("部分结果", "Partial")}</span>
          )}
        </div>
        <span className="treemap-summary">
          <b>
            {provisional ? "≈ " : ""}
            {sizeLabel(total, locale)}
          </b>{" "}
          {provisional
            ? tr("暂估占用", "provisional allocation")
            : isChange ? metricLabel : tr("已统计占用", "accounted for")}
          <i />
          {forest.length.toLocaleString()} {tr("个分区块", "partitions")}
        </span>
      </div>
      <p id={descriptionId} className="treemap-instructions">
        <MousePointer2 size={12} />
        {isChange ? tr(`按${metricLabel}量分配面积。单击查看变化，双击或按 Enter 下钻；方向键切换。`, `Areas compare ${metricLabel}. Click for changes; double-click or Enter to explore. Arrow keys navigate.`) : tr(
          "按图中项目的实际占用分配面积。单击选择，双击或按 Enter 打开；方向键切换。",
          "Areas compare the allocated sizes of the items shown. Click to select; double-click or Enter to open. Arrow keys navigate.",
        )}
      </p>
      <div
        ref={chartRef}
        className="treemap-canvas"
        style={{ height: chartHeight }}
        aria-describedby={showCoverage ? `${descriptionId} ${coverageId}` : descriptionId}
      >
        {!mappedBytes && (
          <div className="treemap-empty">
            {isScanning ? (
              <LoaderCircle size={26} className="treemap-spinner" />
            ) : (
              <Layers size={26} />
            )}
            <strong>
              {isScanning
                ? tr("正在绘制空间地图", "Building your space map")
                : showCoverage
                  ? tr("暂无可绘制的条目", "No items available for the map")
                  : isChange ? tr(`没有可显示的${metricLabel}`, `No ${metricLabel} to display`) : tr("暂无已统计的占用", "No measured allocation to display")}
            </strong>
            {isScanning && <span>{tr("扫描结果会逐步出现在这里。", "Scan results will appear here as they become available.")}</span>}
            {!isScanning && showCoverage && roots.length === 1 && (
              <span>{tr("打开目录明细可查看本层文件和文件夹。", "Open directory details to see this folder’s files and subfolders.")}</span>
            )}
          </div>
        )}
        {tiles.map((tile) => {
          const { node } = tile;
          const isSelected = node.id === chosenPath;
          const matches =
            !normalizedQuery ||
            (node.entry?.path ?? node.inspectPath ?? "").toLocaleLowerCase().includes(normalizedQuery);
          const largeLabel = tile.width >= 85 && tile.height >= 48;
          const visibleLabel = tile.width >= 54 && tile.height >= 23;
          const color = colors[treemapColorIndex(node.branch, colors.length)];
          const style = {
            left: tile.x,
            top: tile.y,
            width: tile.width,
            height: tile.height,
            "--tile-color": color,
            "--tile-depth": tile.depth,
          } as CSSProperties;
          const share = `${isChange ? `${metricLabel} · ` : ""}${percent(node.value)} ${showCoverage ? tr("的图中占用", "of map") : isChange ? tr(`的本层${metricLabel}`, `of scope ${metricLabel}`) : tr("的当前范围占用", "of scope")}`;
          const label = `${nodeName(node)}, ${metric === "growth" ? "+" : ""}${sizeLabel(node.value, locale)}, ${share}${node.synthetic ? `, ${syntheticAction(node)}` : ""}${node.provisional || (node.entry && !node.entry.complete) ? tr("，部分结果", ", partial result") : ""}`;
          const tooltipEvents = {
            onFocus: (event: FocusEvent<HTMLButtonElement>) => {
              setFocusedPath(node.id);
              if (event.currentTarget.matches(":focus-visible")) setHover({ tile });
            },
            onBlur: () => setHover((current) => current?.pointer ? current : undefined),
            onPointerMove: (event: PointerEvent<HTMLButtonElement>) => {
              if (event.pointerType !== "touch") {
                setHover({ tile, pointer: { x: event.clientX, y: event.clientY } });
              }
            },
            onPointerLeave: () => setHover((current) =>
              current?.pointer && current.tile === tile ? undefined : current),
          };
          return (
            <div
              key={node.id}
              data-directory-tile={node.entry && node.entry.kind !== "file" ? node.entry.path : undefined}
              className={`treemap-tile${tile.hasChildren ? " is-parent" : ""}${!largeLabel ? " is-compact" : ""}${node.synthetic ? " is-synthetic" : ""}${isSelected ? " is-selected" : ""}${!matches ? " is-dimmed" : ""}`}
              style={style}
            >
              {node.entry ? (
                <button
                  type="button"
                  className={`treemap-hit${tile.hasChildren ? " is-header" : ""}`}
                  ref={(element) => {
                    if (element) buttons.current.set(node.id, element);
                    else buttons.current.delete(node.id);
                  }}
                  tabIndex={tabPath === node.id ? 0 : -1}
                  aria-label={label}
                  aria-description={entrySummary?.(node.entry)}
                  aria-pressed={isSelected}
                  onClick={() => select(node.entry!, tile)}
                  onDoubleClick={() => activate(node.entry!)}
                  onKeyDown={(event) => moveFocus(event, tile)}
                  {...tooltipEvents}
                >
                  {visibleLabel && (
                    <span className="treemap-tile-label">
                      <span className="treemap-tile-name">
                        {largeLabel &&
                          !tile.hasChildren &&
                          (node.entry.kind === "file" ? (
                            <File size={12} />
                          ) : (
                            <Folder size={12} />
                          ))}
                        <span className="treemap-name-text">{node.name}</span>
                      </span>
                      {(largeLabel ||
                        (tile.hasChildren && tile.width > 120)) && (
                        <span className="treemap-tile-size">
                          {metric === "growth" ? "+" : ""}{sizeLabel(node.value, locale)}
                        </span>
                      )}
                    </span>
                  )}
                  {!tile.hasChildren &&
                    tile.width > 100 &&
                    tile.height > 85 && (
                      <span className="treemap-tile-percent">
                        {percent(node.value)}
                      </span>
                    )}
                  {!tile.hasChildren &&
                    tile.width > 130 &&
                    tile.height > 110 && (
                      <ArrowUpRight
                        className="treemap-open-mark"
                        size={15}
                        aria-hidden="true"
                      />
                    )}
                </button>
              ) : (
                <button
                  type="button"
                  className="treemap-hit treemap-synthetic-hit"
                  ref={(element) => { if (element) buttons.current.set(node.id, element); else buttons.current.delete(node.id); }}
                  tabIndex={tabPath === node.id ? 0 : -1}
                  aria-label={label}
                  onClick={() => expand(node)}
                  onKeyDown={(event) => moveFocus(event, tile)}
                  {...tooltipEvents}
                >
                  {visibleLabel && (
                    <span className="treemap-tile-label">
                      <span className="treemap-tile-name">
                        {tile.width >= 100 && <Layers size={12} aria-hidden="true" />}
                        <span className="treemap-name-text">{nodeName(node)}</span>
                      </span>
                      {largeLabel && (
                        <span className="treemap-tile-size">{metric === "growth" ? "+" : ""}{sizeLabel(node.value, locale)}</span>
                      )}
                      {tile.width >= 145 && tile.height >= 86 && (
                        <span className="treemap-tile-action">{syntheticAction(node)}</span>
                      )}
                    </span>
                  )}
                </button>
              )}
            </div>
          );
        })}
        {hover && hover.tile.node.id !== chosenPath && (
          <div ref={tooltipRef} className="treemap-tooltip" role="tooltip">
            <strong>{nodeName(hover.tile.node)}</strong>
            <span className="treemap-tooltip-path">
              {(hover.tile.node.entry ? entryPathLabel(hover.tile.node.entry) : undefined) ?? hover.tile.node.inspectPath ??
                tr(
                  "该范围内未单独展示的内容",
                  "Contents not individually shown in this scope",
                )}
            </span>
            <div>
              <b>{metric === "reduction" ? `${metricLabel} ` : ""}{metric === "growth" ? "+" : ""}{sizeLabel(hover.tile.node.value, locale)}</b>
              <span>
                {percent(hover.tile.node.value)} {showCoverage ? tr("图中占用", "of map") : isChange ? tr(`本层${metricLabel}`, `of scope ${metricLabel}`) : tr("当前范围", "of scope")}
              </span>
            </div>
            {hover.tile.node.entry && entrySummary ? <small>{entrySummary(hover.tile.node.entry)}</small> : hover.tile.node.synthetic ? (
              <small>
                {hover.tile.node.groupedCount
                  ? tr(
                      `${hover.tile.node.groupedCount} 项合并显示，面积保留。`,
                      `${hover.tile.node.groupedCount} items grouped; their area is preserved.`,
                    )
                  : tr(
                      "未逐项显示的占用。",
                      "Usage not shown as individual blocks.",
                    )}
              </small>
            ) : (
              <small>
                {hover.tile.node.provisional
                  ? tr(
                      "暂估结果 · 父子目录统计尚未一致",
                      "Provisional · parent and child totals are not yet consistent",
                    )
                  : hover.tile.node.entry?.complete
                    ? isChange ? tr("两次扫描之间的变化", "Change between the two scans") : tr("已完成统计", "Scan complete")
                    : tr(
                        "部分结果 · 尚未完成或部分内容无法访问",
                        "Partial · scanning or some contents are inaccessible",
                      )}
                <span>
                  {hover.tile.node.entry?.kind === "file"
                    ? isChange ? tr("双击查看历史变化", "Double-click to inspect historical changes") : tr(
                        "双击在 Finder 中显示",
                        "Double-click to reveal in Finder",
                      )
                    : isChange ? tr("双击查看目录历史", "Double-click to explore folder history") : tr("双击打开文件夹", "Double-click to explore folder")}
                </span>
              </small>
            )}
          </div>
        )}
      </div>
      {showCoverage && (
        <p id={coverageId} className="treemap-coverage" style={{ height: coverageHeight }}>
          {tr(
            `图中显示 ${sizeLabel(mappedBytes, locale)} · 未绘制 ${sizeLabel(unmappedBytes, locale)}`,
            `Map shows ${sizeLabel(mappedBytes, locale)} · ${sizeLabel(unmappedBytes, locale)} not shown`,
          )}
        </p>
      )}
      <div
        className="treemap-legend"
        aria-label={tr("顶层目录图例", "Top-level directory legend")}
      >
        {legend.map((node) => (
          <span key={node.id} title={node.entry ? entryPathLabel(node.entry) : node.inspectPath}>
            <i
              style={{
                background:
                  colors[treemapColorIndex(node.branch, colors.length)],
              }}
            />
            <span>
              {nodeName(node)}
            </span>
            <b>{percent(node.value)}</b>
          </span>
        ))}
        {forest.length > legend.length && (
          <span className="treemap-legend-more">
            +{forest.length - legend.length} {tr("项", "more")}
          </span>
        )}
      </div>
      <div className="treemap-status" aria-live="polite">
        {selected ? (
          <>
            <span className="treemap-status-path" title={entryPathLabel(selected)}>
              {entryPathLabel(selected)}
            </span>
            <b>{sizeLabel(selected.allocatedBytes, locale)}</b>
          </>
        ) : (
          <span>
            {tr(
              isChange ? `仅显示可比较范围内的${metricLabel}。` : "仅比较当前扫描范围内的已统计占用；不代表整块磁盘。",
              isChange ? `Shows ${metricLabel} within comparable paths only.` : "Compares allocated contents in the current scan scope, not the entire disk.",
            )}
          </span>
        )}
        {normalizedQuery && (
          <span>
            {tr(
              "匹配项已高亮 · 面积比例保持不变",
              "Matches highlighted · areas unchanged",
            )}
          </span>
        )}
      </div>
      <Dialog open={!!group} onOpenChange={(open) => { if (!open) setGroup(undefined); }}>
        <DialogContent closeLabel={tr("关闭", "Close")}>
          <DialogHeader><DialogTitle>{tr("合并项目", "Grouped items")}</DialogTitle></DialogHeader>
          <DialogBody>
            {group?.slice(groupPage * 100, (groupPage + 1) * 100).map(node => <Button key={node.id} variant="ghost" className="grouped-item" onClick={() => {
              setGroup(undefined);
              if (node.entry) activate(node.entry); else expand(node);
            }}><span title={node.entry ? entryPathLabel(node.entry) : node.inspectPath}>{nodeName(node)}</span><b>{formatBytes(node.value)}</b></Button>)}
          </DialogBody>
          {group && group.length > 100 && <div className="directory-footer"><span>{groupPage + 1} / {Math.ceil(group.length / 100)}</span><Button disabled={!groupPage} onClick={() => setGroupPage(groupPage - 1)}>{tr("上一页", "Previous")}</Button><Button disabled={(groupPage + 1) * 100 >= group.length} onClick={() => setGroupPage(groupPage + 1)}>{tr("下一页", "Next")}</Button></div>}
        </DialogContent>
      </Dialog>
    </section>
  );
}

export default DiskTreemap;
