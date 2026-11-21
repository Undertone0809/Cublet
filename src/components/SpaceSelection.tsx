import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { motion, useReducedMotion } from "framer-motion";
import { X } from "lucide-react";
import { Button } from "./ui/button";
import { TrashAction } from "./TrashAction";
import { diskInvoke, type PathDetails } from "../lib/disk-api";
import "./space-selection.css";

type Props = {
  name: string;
  path: string;
  locale: "zh" | "en";
  metrics: { label: string; value: ReactNode }[];
  side?: "left" | "right";
  partial?: boolean;
  onClose: () => void;
  onOpen?: () => void;
  openLabel?: string;
  onReveal: () => void;
  historical?: boolean;
  onPermissionRequired?: () => void;
  onTrashed: (path: string, resolvedPath?: string, result?: import("../lib/disk-api").EntryTrashResult) => void;
};

/** One action contract for current allocation and historical growth selections. */
export function SpaceSelection({ name, path, locale, metrics, side = "right", partial, onClose, onOpen, openLabel, onReveal, historical, onPermissionRequired, onTrashed }: Props) {
  const panelRef = useRef<HTMLElement>(null);
  const [retry, setRetry] = useState(0);
  const [metadata, setMetadata] = useState<{ path: string; data?: PathDetails; error?: string }>();
  const details = metadata?.path === path ? metadata : undefined;
  useEffect(() => {
    let active = true;
    setMetadata(undefined);
    void diskInvoke<PathDetails>("get_path_details", { path }).then(data => {
      if (active) setMetadata({ path, data });
    }, cause => { if (active) setMetadata({ path, error: String(cause) }); });
    return () => { active = false; };
  }, [path, retry]);
  const close = () => {
    const scope = panelRef.current?.parentElement?.parentElement;
    scope?.querySelector<HTMLButtonElement>('.treemap-hit[aria-pressed="true"]')?.focus({ preventScroll: true });
    onClose();
  };
  const reducedMotion = useReducedMotion();
  const tr = (zh: string, en: string) => locale === "zh" ? zh : en;
  const date = (timestamp?: number | null) => timestamp && Number.isFinite(timestamp)
    ? new Date(timestamp).toLocaleString(locale === "zh" ? "zh-CN" : "en-US", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" })
    : tr("不可用", "Unavailable");
  return <div className="space-selection-viewport"><motion.aside layout="position" data-side={side} ref={panelRef} onKeyDown={event => { if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); close(); } }} className="space-selection" aria-label={tr("所选项目", "Selected item")}
    style={{ left: side === "left" ? 8 : undefined, right: side === "left" ? "auto" : 8 }}
    initial={reducedMotion ? false : { opacity: 0, x: side === "left" ? "-110%" : "110%" }}
    animate={{ opacity: 1, x: 0 }} transition={{ duration: reducedMotion ? 0 : .3, ease: [.22, 1, .36, 1], layout: { duration: reducedMotion ? 0 : .42, ease: [.22, 1, .36, 1] } }}>
    <div className="selection-heading"><h3><SelectionText identity={path}>{name}</SelectionText></h3><Button variant="ghost" size="icon-sm" aria-label={tr("关闭详情", "Close details")} onClick={close}><X size={14} /></Button></div>
    <code><SelectionText identity={path}>{path}</SelectionText></code>
    <dl>{metrics.map(metric => <div key={metric.label}><dt>{metric.label}</dt><dd><SelectionText identity={path}>{metric.value}</SelectionText></dd></div>)}
      <div className="selection-date"><dt>{historical ? tr("创建时间（当前项目）", "Created (current item)") : tr("创建时间", "Created")}</dt><dd><SelectionText identity={`${path}:${details?.data?.createdMs}`}>{details ? date(details.data?.createdMs) : tr("读取中…", "Loading…")}</SelectionText></dd></div>
      <div className="selection-date"><dt>{historical ? tr("最近修改（当前项目）", "Modified (current item)") : tr("最近修改", "Last modified")}</dt><dd><SelectionText identity={`${path}:${details?.data?.modifiedMs}`}>{details ? date(details.data?.modifiedMs) : tr("读取中…", "Loading…")}</SelectionText></dd></div>
    </dl>
    {details?.error && <div className="selection-feedback"><small role="status">{tr("项目不存在或无法读取日期。", "Item missing or dates could not be read.")}</small><Button size="sm" variant="ghost" onClick={() => setRetry(value => value + 1)}>{tr("重试", "Retry")}</Button></div>}
    {partial && <small>{tr("部分统计", "Partial")}</small>}
    {onOpen && <Button onClick={onOpen}>{openLabel ?? tr("进入目录", "Open folder")}</Button>}
    <Button variant="outline" onClick={onReveal}>{tr("在 Finder 显示", "Show in Finder")}</Button>
    <TrashAction path={path} locale={locale} onTrashed={onTrashed} onPermissionRequired={onPermissionRequired} />
  </motion.aside></div>;
}

/** Immediate text identity; only its pixels transition, never the actionable path. */
function SelectionText({ identity, children }: { identity: string; children: ReactNode }) {
  const element = useRef<HTMLSpanElement>(null);
  const previous = useRef(identity);
  const reducedMotion = useReducedMotion();
  useLayoutEffect(() => {
    const node = element.current;
    if (!node || previous.current === identity) return;
    previous.current = identity;
    if (reducedMotion) return;
    node.classList.add("is-enter-start");
    void node.offsetHeight;
    node.classList.remove("is-enter-start");
  }, [identity, reducedMotion]);
  return <span ref={element} className="t-text-swap">{children}</span>;
}
