import { useEffect, useId, useRef, useState } from "react";
import { Archive, CircleAlert, CircleCheck, FileText, HardDrive, LoaderCircle, Monitor, SlidersHorizontal, Trash2 } from "lucide-react";
import { diskInvoke, type CleanupDisposition } from "../lib/disk-api";
import { formatBytes } from "../lib/utils";
import { Button } from "./ui/button";
import { Checkbox } from "./ui/checkbox";
import { Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "./ui/dialog";

type UninstallKind = "app" | "cache" | "logs" | "preferences" | "data";
type UninstallItem = {
  id: string;
  path: string;
  name: string;
  kind: UninstallKind;
  bytes: number;
  appPath: string;
  blockedReason?: string;
  evidence?: string;
  shared?: boolean;
  copyProtection?: "shared" | "unavailable";
  alreadyRemoved?: boolean;
};
type UninstallPlan = {
  planId: string;
  mode: CleanupDisposition;
  items: UninstallItem[];
  warnings?: string[];
  sharedCopies?: { appPath: string; paths: string[] }[];
};
type UninstallResult = { mode: CleanupDisposition; movedPaths?: string[]; movedAppPaths?: string[]; removedPaths?: string[]; removedAppPaths?: string[]; failures: { path: string; message: string }[] };
type PreviewState =
  | { status: "loading" }
  | { status: "error"; message: string; duringMove?: boolean }
  | { status: "ready"; plan: UninstallPlan }
  | { status: "done"; result: UninstallResult; completedPaths: string[]; retryPlanId?: string };

const groups = [
  { kind: "app", zh: "应用本体", en: "App bundles", Icon: Monitor },
  { kind: "cache", zh: "缓存", en: "Caches", Icon: Archive },
  { kind: "logs", zh: "日志", en: "Logs", Icon: FileText },
  { kind: "preferences", zh: "偏好设置", en: "Preferences", Icon: SlidersHorizontal },
  { kind: "data", zh: "应用数据", en: "Application data", Icon: HardDrive },
] as const;

const uninstallMessages: Record<string, string> = {
  "发现使用同一标识的应用副本，关联数据受保护": "Another copy uses the same identifier. Related data is protected.",
  "应用组容器可能由多个应用使用，成员未核实，数据受保护": "App group membership is unverified. This container is protected.",
  "无法确认是否有其他应用副本，关联数据受保护": "Couldn’t verify other copies. Related data is protected.",
  "应用或辅助进程正在使用这些项目，请退出应用后重新预览": "An app or helper is using these items. Quit it and check again.",
  "无法确认应用运行状态，请重新预览": "Couldn’t verify whether the app is running. Check again.",
  "项目已发生变化，请重新预览": "This item changed. Review it again before proceeding.",
  "卸载预览已失效，请重新预览": "This preview expired. Review the items again.",
  "处理方式已变更或确认方式不匹配，请重新预览": "The cleanup action changed. Review and confirm it again.",
  "请先永久删除对应应用": "Related data was kept because its app bundle wasn’t deleted.",
  "请先将对应应用移到废纸篓": "Related data was kept because its app bundle wasn’t moved to Trash.",
  "无法核实永久删除的目录边界，请重新预览": "Couldn’t verify which contents can be permanently deleted. Review again.",
  "沙盒容器归属无法核实": "Couldn’t verify who owns this app container.",
  "目录名称匹配另一个已安装应用的标识": "The folder name matches another installed app’s identifier.",
  "其他用户拥有的关联数据受保护": "This data belongs to another user and is protected.",
  "磁盘管家自身受保护": "The running copy of Cublet is protected.",
  "部分项目无法估算大小，列表中的大小可能偏低": "Some sizes couldn’t be measured, so the estimate may be low.",
};

type Props = {
  paths: string[];
  locale: "zh" | "en";
  onClose: () => void;
  onMoved: (paths: string[], appPaths: string[]) => void;
  returnFocus: () => HTMLElement | null;
};

export function AppUninstallDialog({ paths, locale, onClose, onMoved, returnFocus }: Props) {
  const tr = (zh: string, en: string) => locale === "zh" ? zh : en;
  function explain(message: string) {
    const detail = message.replace(/^Error invoking remote method '[^']+': Error: /, "");
    if (locale === "en") return uninstallMessages[detail] ?? detail;
    if (detail === "Original item was restored: This item changed. Review it again before proceeding.") {
      return "检查到项目已变化，已放回原位置。请重新预览。";
    }
    return detail;
  }
  function evidenceLabel(evidence: string) {
    const labels: Record<string, [string, string]> = {
      "verified-independent-bundle": ["已核实为独立应用", "Verified standalone application"],
      "exact-bundle-id": ["路径与应用标识精确匹配", "Path matches this app’s exact bundle identifier"],
      "embedded-helper-bundle-id": ["匹配应用内辅助程序的标识", "Matches an embedded helper’s bundle identifier"],
      "embedded-squirrel-updater": ["应用内包含对应的更新器", "This app contains the matching updater"],
      "exact-byhost-owner": ["设备偏好文件与应用标识匹配", "Device preference file matches the app identifier"],
      "darwin-current-user-exact-id": ["当前用户缓存或临时目录与应用标识匹配", "Current user’s cache or temporary folder matches the app identifier"],
      "container-metadata-exact-id": ["容器元数据确认属于此应用", "Container metadata confirms this app’s ownership"],
      "container-owner-unverified": ["容器位置匹配，归属尚未核实", "Container location matches; ownership is unverified"],
      "declared-group-container": ["应用声明了这个共享容器", "The app declares this group container"],
      "application-name-only": ["仅匹配应用名称", "Matched by app name only"],
    };
    const value = labels[evidence];
    return value ? tr(...value) : evidence;
  }
  const [open, setOpen] = useState(true);
  const [state, setState] = useState<PreviewState>({ status: "loading" });
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [attempt, setAttempt] = useState(0);
  const [moving, setMoving] = useState(false);
  const requestRef = useRef<{ attempt: number; promise: Promise<UninstallPlan> }>(null);
  const selectionToRestore = useRef<Map<string, boolean> | null>(null);
  const retryPlanId = useRef<string>(undefined);
  const prefix = useId();
  const mode = state.status === "done" ? state.result.mode : state.status === "ready" ? state.plan.mode : undefined;
  const permanent = mode === "permanent";
  useEffect(() => {
    if (!open) return;
    if (requestRef.current?.attempt !== attempt) {
      requestRef.current = { attempt, promise: diskInvoke<UninstallPlan>("preview_app_uninstall", { paths, retryPlanId: retryPlanId.current }) };
    }
    let ignore = false;
    requestRef.current.promise.then((plan) => {
      if (ignore) return;
      const previous = selectionToRestore.current;
      const checkedApps = new Set(plan.items.filter((item) => item.kind === "app" && !item.blockedReason
        && (previous?.get(item.path) ?? true)).map((item) => item.appPath));
      for (const item of plan.items) if (item.alreadyRemoved) checkedApps.add(item.appPath);
      setSelected(new Set(plan.items.filter((item) => !item.blockedReason && checkedApps.has(item.appPath)
        && !item.alreadyRemoved && (item.kind === "app" || previous?.get(item.path))).map((item) => item.id)));
      selectionToRestore.current = null;
      setState({ status: "ready", plan });
    }).catch((error: unknown) => {
      if (!ignore) setState({ status: "error", message: String(error) });
    });
    return () => { ignore = true; };
  }, [attempt, open, paths]);

  const plan = state.status === "ready" ? state.plan : null;
  const selectedItems = plan?.items.filter((item) => selected.has(item.id)) ?? [];
  const selectedBytes = selectedItems.reduce((sum, item) => sum + item.bytes, 0);
  const bundles = new Map(plan?.items.filter((item) => item.kind === "app").map((item) => [item.appPath, item]));
  const visibleItems = plan?.items.filter((item) => item.copyProtection !== "unavailable") ?? [];
  const sharedCopyPaths = [...new Set(plan?.sharedCopies?.flatMap((copy) => copy.paths) ?? [])];
  const selectableCaches = visibleItems.filter(item => item.kind === "cache" && item.shared === false && !item.blockedReason
    && (bundles.get(item.appPath)?.alreadyRemoved || selected.has(bundles.get(item.appPath)?.id ?? "")));

  function retry() {
    if (state.status === "done") retryPlanId.current = state.retryPlanId;
    selectionToRestore.current = plan ? new Map(plan.items.filter((item) => !item.blockedReason)
      .map((item) => [item.path, selected.has(item.id)])) : null;
    setSelected(new Set());
    setState({ status: "loading" });
    setAttempt((value) => value + 1);
  }

  function choose(item: UninstallItem, checked: boolean) {
    if (!plan) return;
    setSelected((current) => {
      const next = new Set(current);
      if (checked) {
        const bundle = bundles.get(item.appPath);
        if (item.kind !== "app" && !bundle?.alreadyRemoved && !current.has(bundle?.id ?? "")) return current;
        next.add(item.id);
      } else {
        next.delete(item.id);
        if (item.kind === "app") {
          for (const candidate of plan.items) if (candidate.appPath === item.appPath) next.delete(candidate.id);
        }
      }
      return next;
    });
  }

  async function executeUninstall() {
    if (!plan || moving || !selectedItems.length) return;
    setMoving(true);
    try {
      const result = await diskInvoke<UninstallResult>("execute_app_uninstall", {
        planId: plan.planId,
        selectedIds: selectedItems.map((item) => item.id),
        confirmed: true,
        mode: plan.mode,
      });
      const completedPaths = result.removedPaths ?? result.movedPaths ?? [];
      const completedAppPaths = result.removedAppPaths ?? result.movedAppPaths ?? [];
      setState({ status: "done", result, completedPaths, retryPlanId: completedAppPaths.length || retryPlanId.current ? plan.planId : undefined });
      onMoved(completedPaths, completedAppPaths);
    } catch (error) {
      setState({ status: "error", message: String(error), duringMove: true });
    } finally {
      setMoving(false);
    }
  }

  return (
    <Dialog open={open} disablePointerDismissal onOpenChange={(next) => { if (!moving) setOpen(next); }}
      onOpenChangeComplete={(next) => { if (!next) onClose(); }}>
      <DialogContent className="app-uninstall-dialog" closeLabel={tr("关闭", "Close")} showCloseButton={!moving} finalFocus={returnFocus}>
        <DialogHeader>
          <DialogTitle>{state.status === "done" ? tr("卸载结果", "Uninstall results") : tr("预览卸载", "Review uninstall")}</DialogTitle>
          <DialogDescription>
            {state.status === "done"
              ? permanent ? tr("已永久删除的项目无法从废纸篓恢复。下方列出已完成和未完成的项目。", "Permanently deleted items cannot be restored from Trash. Completed and unfinished items are listed below.")
                : tr("已移动的项目可在 Finder 废纸篓中找到。下方列出已完成和未完成的项目。", "Moved items are in Finder Trash. Completed and unfinished items are listed below.")
              : tr(`已选择 ${paths.length} 个应用。可处理的应用本体默认勾选，请按需选择相关文件。`, `${paths.length} ${paths.length === 1 ? "app" : "apps"} selected. Eligible app bundles are checked by default. Choose any related files you want to remove.`)}
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="app-uninstall-body" aria-busy={moving || state.status === "loading"}>
          {state.status === "loading" && (
            <p className="app-uninstall-status" role="status"><LoaderCircle className="spin" size={15} />{tr("正在查找相关文件…", "Looking for related files…")}</p>
          )}
          {state.status === "error" && (
            <div className="app-uninstall-status app-uninstall-error" role="alert">
              <CircleAlert size={16} />
              <div>
                <strong>{state.duringMove ? tr("无法确认完整卸载结果，请重新核对。", "Couldn’t confirm the full uninstall result. Review again.") : tr("无法生成卸载预览。", "Could not prepare the uninstall preview.")}</strong>
                <p>{explain(state.message)}</p>
                {state.duringMove && <p>{tr("本次未收到完整结果。重新预览会检查仍存在的项目，再决定是否继续。", "A complete result was not received. Preview again to check which items still exist before continuing.")}</p>}
              </div>
            </div>
          )}
          {plan && (
            <>
              <div className="app-uninstall-status" role="status"><CircleAlert size={16} /><div>
                <strong>{permanent ? tr("本次操作：直接永久删除", "This action: delete permanently") : tr("本次操作：移到废纸篓", "This action: move to Trash")}</strong>
                <p>{permanent ? tr("所选项目会直接删除，无法从废纸篓恢复。", "Selected items will be permanently deleted and cannot be restored from Trash.") : tr("所选项目会移入废纸篓，清空废纸篓后才会释放空间。", "Selected items will move to Trash. Empty Trash to free disk space.")}</p>
              </div></div>
              {!!plan.warnings?.length && (
                <ul className="app-uninstall-warnings" aria-label={tr("预览提示", "Preview notices")}>
                  {plan.warnings.map((warning, index) => <li key={index}><CircleAlert size={14} aria-hidden="true" />{explain(warning)}</li>)}
                </ul>
              )}
              {sharedCopyPaths.length > 0 && (
                <div className="app-uninstall-status" role="status" id={`${prefix}-shared-copies`}>
                  <CircleAlert size={16} aria-hidden="true" />
                  <div>
                    <strong>{tr("发现同标识的应用副本，共享数据会保留。", "Other copies were found. Shared data will be kept.")}</strong>
                    {sharedCopyPaths.map((path) => <p key={path}><code className="app-uninstall-copy-path">{path}</code></p>)}
                  </div>
                </div>
              )}
              {!visibleItems.length && <p className="app-uninstall-status" role="status">{tr("没有找到可预览的项目。", "No items found to preview.")}</p>}
              {groups.map(({ kind, zh, en, Icon }) => {
                const items = visibleItems.filter((item) => item.kind === kind);
                if (!items.length) return null;
                return (
                  <section className="app-uninstall-group" aria-labelledby={`${prefix}-${kind}`} key={kind}>
                    <h3 id={`${prefix}-${kind}`}>
                      <Icon size={15} aria-hidden="true" />{tr(zh, en)}
                      <span>{items.length}</span>
                      {kind !== "app" && items.some((item) => !item.blockedReason) && <small>{tr("可选", "Optional")}</small>}
                      {kind === "cache" && selectableCaches.length > 0 && <Button variant="ghost" size="sm" disabled={moving}
                        onClick={() => setSelected(previous => new Set([...previous, ...selectableCaches.map(item => item.id)]))}>
                        {tr("勾选可清理缓存", "Select eligible caches")}
                      </Button>}
                    </h3>
                    <ul>
                      {items.map((item, index) => {
                        const bundle = bundles.get(item.appPath);
                        const needsBundle = kind !== "app" && !bundle?.alreadyRemoved && !selected.has(bundle?.id ?? "");
                        const sharedWithCopy = item.copyProtection === "shared";
                        const reason = item.alreadyRemoved ? tr("应用本体已处理。", "App bundle already removed.")
                          : sharedWithCopy ? undefined
                            : item.blockedReason || (needsBundle ? tr("请先勾选对应的应用本体。", "Select its app bundle first.") : undefined);
                        const id = `${prefix}-${kind}-${index}`;
                        return (
                          <li className="app-uninstall-item" key={item.id}>
                            <div className="app-uninstall-item-control">
                              <Checkbox
                                checked={selected.has(item.id)}
                                disabled={moving || needsBundle || item.alreadyRemoved || !!item.blockedReason}
                                aria-labelledby={`${id}-name`}
                                aria-describedby={`${id}-path${reason ? ` ${id}-reason` : ""}${sharedWithCopy ? ` ${prefix}-shared-copies` : ""}`}
                                onCheckedChange={(checked) => choose(item, checked)}
                              />
                            </div>
                            <div className="app-uninstall-item-detail">
                              <div>
                                <strong id={`${id}-name`}>{item.name}</strong>
                                {kind !== "app" && paths.length > 1 && bundle && <span className="app-uninstall-owner">{bundle.name}</span>}
                              </div>
                              <code id={`${id}-path`}>{item.path}</code>
                              {item.evidence && (!reason || !item.blockedReason) && <small>{evidenceLabel(item.evidence)}</small>}
                              {reason && <small id={`${id}-reason`} className={item.blockedReason ? "app-uninstall-blocked" : undefined}>{item.blockedReason && <CircleAlert size={12} aria-hidden="true" />}{explain(reason)}</small>}
                            </div>
                            <span className="app-uninstall-size">{item.blockedReason && item.bytes === 0 ? "—" : formatBytes(item.bytes)}</span>
                          </li>
                        );
                      })}
                    </ul>
                  </section>
                );
              })}
            </>
          )}
          {state.status === "done" && (
            <>
              <div className="app-uninstall-status" role="status">
                {state.result.failures.length ? <CircleAlert size={16} /> : <CircleCheck size={16} />}
                <div>
                  <strong>{permanent ? tr(`已永久删除：${state.completedPaths.length} 项`, `Permanently deleted: ${state.completedPaths.length}`) : tr(`已移入废纸篓：${state.completedPaths.length} 项`, `Moved to Trash: ${state.completedPaths.length}`)}</strong>
                  {!!state.result.failures.length && <p>{tr(`未完成：${state.result.failures.length} 项`, `Unfinished: ${state.result.failures.length}`)}</p>}
                </div>
              </div>
              {!!state.result.failures.length && (
                <ul className="app-uninstall-failures" aria-label={tr("未完成的项目", "Unfinished items")}>
                  {state.result.failures.map(({ path, message }, index) => (
                    <li key={`${path}-${index}`}><code>{path}</code><span>{explain(message)}</span></li>
                  ))}
                </ul>
              )}
              {!!state.completedPaths.length && (
                <details className="app-uninstall-moved">
                  <summary>{tr("查看已处理项目的原始路径", "Show original paths of completed items")}</summary>
                  <ul>{state.completedPaths.map((path) => <li key={path}><code>{path}</code></li>)}</ul>
                </details>
              )}
            </>
          )}
        </DialogBody>
        <DialogFooter className="app-uninstall-footer">
          {plan && (
            <div className="app-uninstall-summary">
              <strong aria-live="polite">{tr(`${selectedItems.length} 项 · ${formatBytes(selectedBytes)}`, `${selectedItems.length} ${selectedItems.length === 1 ? "item" : "items"} · ${formatBytes(selectedBytes)}`)}</strong>
              <small>{permanent ? tr("永久删除，无法从废纸篓恢复。", "Permanent deletion. Cannot be restored from Trash.") : tr("移入废纸篓不会释放磁盘空间。", "Moving to Trash does not free disk space.")}</small>
            </div>
          )}
          <Button variant="outline" disabled={moving} onClick={() => setOpen(false)}>
            {state.status === "done" ? tr("完成", "Done") : tr("取消", "Cancel")}
          </Button>
          {state.status === "error" && <Button onClick={retry}>{tr("重新预览", "Preview again")}</Button>}
          {state.status === "done" && !!state.result.failures.length && <Button onClick={retry}>{tr("重新预览剩余项目", "Review remaining items")}</Button>}
          {plan && <Button variant="outline" disabled={moving} onClick={retry}>{tr("重新检查", "Check again")}</Button>}
          {plan && (
            <Button variant="destructive" disabled={moving || !selectedItems.length} onClick={() => void executeUninstall()}>
              {moving ? <LoaderCircle className="spin" aria-hidden="true" /> : <Trash2 aria-hidden="true" />}
              {moving ? permanent ? tr("正在删除…", "Deleting…") : tr("正在移动…", "Moving…") : permanent ? tr("永久删除", "Delete permanently") : tr("移到废纸篓", "Move to Trash")}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
