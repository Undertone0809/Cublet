import { useEffect, useRef, useState } from "react";
import { LoaderCircle, Trash2 } from "lucide-react";
import { Button } from "./ui/button";
import { Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "./ui/dialog";
import { diskInvoke, getTrashPath, type EntryTrashResult, type EntryTrashPreview } from "../lib/disk-api";

type Props = { path: string; locale: "zh" | "en"; iconOnly?: boolean; name?: string; onTrashed: (path: string, resolvedPath?: string, result?: EntryTrashResult) => void; onPermissionRequired?: () => void };

/** User-directed native Trash; progress and path classification do not gate intent. */
export function TrashAction({ path, locale, iconOnly, name, onTrashed, onPermissionRequired }: Props) {
  const [trashPath, setTrashPath] = useState<string>();
  useEffect(() => { void getTrashPath().then(setTrashPath).catch(() => {}); }, []);
  const emptyTrash = !!trashPath && path.replace(/\/+$/, "") === trashPath;
  const trashTrigger = useRef<HTMLButtonElement>(null);
  const [preview, setPreview] = useState<EntryTrashPreview>();
  const [previewNeedsRefresh, setPreviewNeedsRefresh] = useState(false);
  const [preparing, setPreparing] = useState(false);
  const [moving, setMoving] = useState(false);
  const movingRef = useRef(false);
  const previewRequest = useRef(0);
  const currentPath = useRef(path);
  currentPath.current = path;
  const [actionError, setActionError] = useState("");
  useEffect(() => {
    setPreparing(false);
    setActionError("");
    setPreviewNeedsRefresh(false);
    if (!movingRef.current) setPreview(undefined);
    return () => { ++previewRequest.current; };
  }, [path]);
  const tr = (zh: string, en: string) => locale === "zh" ? zh : en;
  const completion = useRef(onTrashed);
  completion.current = onTrashed;
  async function prepareTrash() {
    if (preparing || movingRef.current) return;
    const request = ++previewRequest.current;
    setPreparing(true);
    setActionError("");
    try {
      const result = await diskInvoke<EntryTrashPreview>("preview_trash_entry", { path });
      if (previewRequest.current === request && currentPath.current === path) { setPreview(result); setPreviewNeedsRefresh(false); }
    } catch (cause) { if (previewRequest.current === request && currentPath.current === path) setActionError(String(cause)); }
    finally { if (previewRequest.current === request && currentPath.current === path) setPreparing(false); }
  }
  function dismissPreview() {
    if (movingRef.current) return;
    ++previewRequest.current;
    setPreparing(false);
    setPreview(undefined);
  }
  async function moveToTrash() {
    if (!preview || movingRef.current) return;
    movingRef.current = true;
    setMoving(true);
    setActionError("");
    try {
      const result = await diskInvoke<EntryTrashResult>("trash_entry", { planId: preview.planId, confirmed: true, action: preview.action ?? "move-to-trash" });
      setPreview(undefined);
      completion.current(preview.path, result?.resolvedPath, result);
    } catch (cause) { setActionError(String(cause)); setPreviewNeedsRefresh(true); }
    finally { movingRef.current = false; setMoving(false); }
  }
  return <>
    <Button ref={trashTrigger} variant={iconOnly ? "ghost" : "destructive"} size={iconOnly ? "icon-sm" : "default"} disabled={preparing || moving} onClick={() => void prepareTrash()} aria-label={iconOnly ? `${emptyTrash ? tr("清空废纸篓", "Empty Trash") : tr("移到废纸篓", "Move to Trash")} ${name ?? path}` : undefined}>
      {preparing ? <LoaderCircle className="spin" /> : <Trash2 />}{!iconOnly && (emptyTrash ? tr("清空废纸篓", "Empty Trash") : tr("移到废纸篓", "Move to Trash"))}
    </Button>
    {actionError && !preview && <div className="selection-feedback"><small role="alert">{actionError}</small>{actionError.includes("permission-required:") && onPermissionRequired && <Button variant="outline" onClick={onPermissionRequired}>{tr("设置访问权限", "Set up access")}</Button>}</div>}
    <Dialog open={!!preview} onOpenChange={open => { if (!open) dismissPreview(); }}>
      <DialogContent showCloseButton={!moving} closeLabel={tr("关闭", "Close")} finalFocus={trashTrigger}
        onKeyDown={event => event.stopPropagation()}>
        <DialogHeader><DialogTitle>{preview?.action === "empty-trash" ? tr("确认清空废纸篓？", "Empty Trash permanently?") : tr("确认移到废纸篓？", "Move this item to Trash?")}</DialogTitle>
          <DialogDescription>{preview?.action === "empty-trash" ? tr("将永久删除当前用户废纸篓中本次检查的内容，无法在 Finder 中恢复。不会清空其他磁盘的废纸篓；检查后新增的顶层项目会保留。", "Permanently delete the checked contents of this user’s Trash. You cannot restore them in Finder. Trash on other disks and top-level items added after this check are kept.") : tr("可在 Finder 的废纸篓中恢复。移到废纸篓不会释放磁盘空间；需要之后清空废纸篓才会真正释放。", "You can restore it from Finder Trash. Moving it to Trash does not free disk space; empty Trash afterwards to release it.")}</DialogDescription>
        </DialogHeader>
        <DialogBody><p><strong>{preview?.name}</strong></p><code className="wrap-path">{preview?.path}</code>
          {preview?.action === "empty-trash" && <p>{tr(`本次检查到 ${preview.itemCount ?? 0} 个顶层项目。`, `${preview.itemCount ?? 0} top-level items checked.`)}</p>}
          {preview?.kind === "directory" && preview.action !== "empty-trash" && <p>{tr("此目录及其全部内容都会移到废纸篓。", "This folder and all its contents will be moved to Trash.")}</p>}
          {actionError && <p role="alert">{actionError}</p>}
          {actionError.includes("permission-required:") && onPermissionRequired && <Button variant="outline" onClick={() => { dismissPreview(); onPermissionRequired(); }}>{tr("设置访问权限", "Set up access")}</Button>}
        </DialogBody>
        <DialogFooter><Button variant="outline" disabled={moving} onClick={dismissPreview}>{tr("取消", "Cancel")}</Button>
          <Button variant={previewNeedsRefresh ? "outline" : "destructive"} disabled={moving || preparing} onClick={() => { if (previewNeedsRefresh) void prepareTrash(); else void moveToTrash(); }}>{moving || preparing ? <LoaderCircle className="spin" /> : <Trash2 />}{moving ? (preview?.action === "empty-trash" ? tr("正在清空…", "Emptying…") : tr("正在移动…", "Moving…")) : preparing ? tr("正在检查…", "Checking…") : previewNeedsRefresh ? tr("重新检查", "Check again") : preview?.action === "empty-trash" ? tr("永久删除", "Delete Permanently") : tr("确认移到废纸篓", "Confirm Move to Trash")}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  </>;
}
