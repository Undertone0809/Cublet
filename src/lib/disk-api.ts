export type DiskEntry = {
  path: string;
  name: string;
  parentPath: string;
  kind: "directory" | "file" | "application";
  allocatedBytes: number;
  logicalBytes: number;
  fileCount: number;
  directoryCount: number;
  complete: boolean;
};
export type DiskApplication = {
  path: string;
  name: string;
  bundleId?: string;
  version?: string;
  bundleBytes: number;
  bundleBytesKnown?: boolean;
  supportBytes: number;
  cacheBytes: number;
  sharedData: boolean;
};
export type ApplicationSearchResult = {
  applications: DiskApplication[];
  partial: boolean;
  notices: string[];
};
export type CleanupDisposition = "permanent" | "trash";
export type CleanupDispositionSettings = { mode: CleanupDisposition; revision: string };
export const getCleanupDisposition = () => diskInvoke<CleanupDispositionSettings>("get_cleanup_disposition");
export const setCleanupDisposition = (mode: CleanupDisposition) => diskInvoke<CleanupDispositionSettings>("set_cleanup_disposition", { mode });
export type DiskScan = {
  scanId: string;
  status: "running" | "complete" | "cancelled" | "failed";
  roots: string[];
  startedAtMs: number;
  updatedAtMs: number;
  usageStale?: boolean;
  elapsedMs: number;
  scannedFiles: number;
  scannedDirectories: number;
  allocatedBytes: number;
  logicalBytes: number;
  currentPath: string;
  entries: DiskEntry[];
  treemapEntries?: DiskEntry[];
  applications: DiskApplication[];
  issues: { path: string; kind: string; message: string }[];
  issueCount: number;
  cacheHit: boolean;
  cacheAgeMs: number;
  coverageComplete: boolean;
  scopeNote: string;
  sourceRoots?: string[];
  viewPath?: string;
  viewDetailComplete?: boolean;
  sourceScanId?: string;
  scanMethod?: "full" | "incremental" | "validated-reuse" | "cached";
  freshness?: "cached" | "updating" | "verified" | "partial";
  fallbackReason?: string;
};
export type DiskDirectoryItem = {
  path: string;
  name: string;
  parentPath: string;
  kind: "directory" | "file" | "application" | "symlink" | "unknown";
  allocatedBytes?: number;
  logicalBytes?: number;
  measurement: "file" | "summary" | "unknown" | "excluded";
  complete: boolean;
  hardLinkIdentity?: string;
};
export type DiskDirectoryPage = {
  scanId: string;
  path: string;
  sourceUpdatedAtMs: number;
  listedAtMs: number;
  entries: DiskDirectoryItem[];
  hasMore: boolean;
  nextAfterPath?: string;
  issues: DiskScan["issues"];
  scopeNote: string;
};
export type DiskOverview = {
  capacity: {
    path: string;
    totalBytes: number;
    availableBytes: number;
    usedBytes: number;
  };
  defaultRoots: { path: string; label: string }[];
  scopeNote: string;
};
export async function diskInvoke<T>(
  method: string,
  params: Record<string, unknown> = {},
): Promise<T> {
  if (!window.electronAPI) throw new Error("请在桌面应用中使用真实磁盘分析。");
  return window.electronAPI.invoke<T>(method, params);
}
export const diskOverview = () => diskInvoke<DiskOverview>("disk_overview");
export type DiskScanMode = "auto" | "full";
export const startDiskScan = (
  path?: string,
  refresh = false,
  allowProtected = false,
  mode: DiskScanMode = "auto",
) => diskInvoke<DiskScan>("start_disk_scan", { path, refresh, allowProtected, mode });
export const getDiskScan = (scanId: string) =>
  diskInvoke<DiskScan>("get_disk_scan", { scanId });
export const getDiskScanView = (scanId: string, path: string) =>
  diskInvoke<DiskScan>("get_disk_scan_view", { scanId, path });
export const cancelDiskScan = (scanId: string) =>
  diskInvoke<DiskScan>("cancel_disk_scan", { scanId });
export const inspectDiskDirectory = (scanId: string, path: string) =>
  diskInvoke<DiskScan>("inspect_disk_directory", { scanId, path });
export const getCachedDiskScan = (path?: string, allowProtected = false, retainScanId?: string) =>
  diskInvoke<DiskScan | null>("get_cached_disk_scan", { path, allowProtected, retainScanId });
export const listDiskDirectory = (scanId: string, path: string, afterPath?: string) =>
  diskInvoke<DiskDirectoryPage>("list_disk_directory", { scanId, path, afterPath, limit: 200 });

/** Live metadata, deliberately separate from cached scan measurements. */
export type PathDetails = {
  path: string;
  createdMs: number | null;
  modifiedMs: number | null;
  kind: string;
  canTrash: boolean;
  trashUnavailableReason?: string;
};
export type EntryTrashResult = { resolvedPath?: string; action?: "empty-trash"; removedCount?: number; remainingCount?: number; failures?: { name: string; message: string }[] };
export type EntryTrashPreview = { planId: string; path: string; name: string; kind: string; action?: "empty-trash"; itemCount?: number };
let trashPathRequest: Promise<string> | undefined;
export const getTrashPath = () => trashPathRequest ??= diskInvoke<string>("get_trash_path").catch(error => { trashPathRequest = undefined; throw error; });

/** Additive, disjoint changed partitions; directory rows can be aggregate residuals. */
export type DiskScanComparisonEntry = {
  path: string;
  name: string;
  parentPath: string;
  kind: "directory" | "file";
  beforeBytes: number;
  afterBytes: number;
  growthBytes: number;
  reductionBytes: number;
  netBytes: number;
  detailLimited: boolean;
};
export type DiskScanComparison = {
  status: "ready" | "no-baseline" | "scanning" | "unavailable" | "baseline-unavailable";
  baselineId?: string;
  currentId?: string;
  refreshing?: boolean;
  baselineStartedAtMs?: number;
  baselineUpdatedAtMs?: number;
  currentStartedAtMs: number;
  currentUpdatedAtMs: number;
  roots: string[];
  entries: DiskScanComparisonEntry[];
  growthBytes: number;
  reductionBytes: number;
  netBytes: number;
  limited: boolean;
};
export const getDiskScanComparison = (scanId: string, path?: string, baselineId?: string) =>
  diskInvoke<DiskScanComparison>("get_disk_scan_comparison", { scanId, path, ...(baselineId ? { baselineId } : {}) });
export const listDiskScanHistory = (scanId: string) =>
  diskInvoke<import('./growth-history').GrowthHistory>("list_disk_scan_history", { scanId });

export type DiskCacheSettings = {
  limitMiB: number;
  usedBytes: number;
};
export const getDiskCacheSettings = () =>
  diskInvoke<DiskCacheSettings>("get_disk_cache_settings");
export const setDiskCacheLimit = (limitMiB: number) =>
  diskInvoke<DiskCacheSettings>("set_disk_cache_limit", { limitMiB });
