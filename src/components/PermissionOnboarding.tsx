import { useState } from "react";
import { ArrowUpRight, Folder } from "lucide-react";
import { Button } from "./ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { diskInvoke } from "../lib/disk-api";
import {
  openDiskPrivacySettings,
  type DiskPermissionInfo,
  type PermissionChoice,
} from "../lib/permissions";
import { useI18n } from "../lib/i18n";
import "../permission-compact.css";
export function PermissionOnboarding({
  open,
  onClose,
  onChoose,
  info,
  canClose,
  busy,
}: {
  open: boolean;
  onClose: () => void;
  onChoose: (choice: PermissionChoice) => void;
  info: DiskPermissionInfo | null;
  canClose: boolean;
  busy: boolean;
}) {
  const { locale } = useI18n();
  const tr = (zh: string, en: string) => (locale === "zh" ? zh : en);
  const [opened, setOpened] = useState(false);
  const [error, setError] = useState("");
  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!value && canClose && !busy) onClose();
      }}
    >
      <DialogContent
        className="permission-dialog"
        showCloseButton={canClose && !busy}
        closeLabel={tr("关闭", "Close")}
      >
        <DialogHeader>
          <DialogTitle>{tr("磁盘访问权限", "Disk access")}</DialogTitle>
          <DialogDescription>
            {tr(
              "完整分析需要完全磁盘访问权限。基础扫描会跳过受保护目录。",
              "Full analysis needs Full Disk Access. A basic scan skips protected folders.",
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="permission-body">
          <p className="permission-current-state" role="status">
            {info?.status === "granted"
              ? tr("已获得完全磁盘访问权限", "Full Disk Access is available")
              : info?.status === "denied"
                ? tr("当前磁盘访问受限", "Disk access is restricted")
                : tr("尚未确认磁盘访问权限", "Disk access is unconfirmed")}
          </p>
          <ol className="permission-steps">
            <li>
              <span aria-hidden="true">1</span>
              <div>
                <strong>
                  {tr("打开系统隐私设置", "Open privacy settings")}
                </strong>
                <p>
                  {tr(
                    "系统设置 → 隐私与安全性 → 完全磁盘访问权限",
                    "System Settings → Privacy & Security → Full Disk Access",
                  )}
                </p>
              </div>
            </li>
            <li>
              <span aria-hidden="true">2</span>
              <div>
                <strong>
                  {tr(
                    "加入并开启 Cublet",
                    "Add and enable Cublet",
                  )}
                </strong>
                <p>
                  {tr(
                    "若应用不在列表中，点击“+”或从 Finder 拖入。",
                    "If the app is missing, add it with + or drag it from Finder.",
                  )}
                </p>
              </div>
            </li>
            <li>
              <span aria-hidden="true">3</span>
              <div>
                <strong>
                  {tr("返回应用并继续", "Return to the app and continue")}
                </strong>
                <p>
                  {tr(
                    "如 macOS 提示，请重新打开应用。",
                    "Reopen the app if macOS asks.",
                  )}
                </p>
              </div>
            </li>
          </ol>
          <div className="permission-open-actions">
            <Button
              variant="outline"
              disabled={busy}
              onClick={async () => {
                setError("");
                try {
                  const result = await openDiskPrivacySettings();
                  if (result.opened) setOpened(true);
                  else
                    setError(
                      tr(
                        "未能打开系统设置，请按上方路径手动打开。",
                        "Open System Settings manually using the path above.",
                      ),
                    );
                } catch (e) {
                  setError(String(e));
                }
              }}
            >
              {tr("打开系统设置", "Open System Settings")}
              <ArrowUpRight size={14} />
            </Button>
            <Button
              variant="ghost"
              disabled={busy || !info?.appPath}
              onClick={async () => {
                try {
                  await diskInvoke("reveal_path", { path: info?.appPath });
                } catch (e) {
                  setError(String(e));
                }
              }}
            >
              <Folder size={14} />
              {tr("在 Finder 显示应用", "Show app in Finder")}
            </Button>
          </div>
          {opened && (
            <p className="permission-status" role="status">
              {tr(
                "系统设置已打开。",
                "System Settings opened.",
              )}
            </p>
          )}
          {error && (
            <p className="error-banner" role="alert">
              {error}
            </p>
          )}
        </DialogBody>
        <DialogFooter>
          <Button
            variant="outline"
            disabled={busy}
            onClick={() => onChoose("limited")}
          >
            {tr("使用基础扫描", "Use basic scan")}
          </Button>
          <Button disabled={busy} onClick={() => onChoose("full")}>
            {tr("完成设置后继续", "Continue after setup")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
