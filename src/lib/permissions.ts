import { diskInvoke } from "./disk-api";
export type PermissionChoice = "limited" | "full";
export type DiskPermissionInfo = {
  status: "granted" | "denied" | "unknown";
  canVerify: boolean;
  checkedAt?: string;
  evidence?: { kind: string; result: string; code?: string };
  platform: string;
  appPath: string;
  appName: string;
  isPackaged: boolean;
  relaunchRecommended: boolean;
  details: string[];
};
export const PERMISSION_CHOICE_KEY = "mac-storage-manager.permission-choice.v1";
export function readPermissionChoice(): PermissionChoice | null {
  try {
    const choice = window.localStorage.getItem(PERMISSION_CHOICE_KEY);
    return choice === "limited" || choice === "full" ? choice : null;
  } catch {
    return null;
  }
}
export const getDiskPermissions = (refresh = false) =>
  diskInvoke<DiskPermissionInfo>("get_disk_permissions", { refresh });
export const openDiskPrivacySettings = () =>
  diskInvoke<{ opened: boolean; relaunchRecommended: boolean }>(
    "open_disk_privacy_settings",
  );
