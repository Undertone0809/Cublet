import type { DiskScan } from "./disk-api";

export const SCAN_FRESHNESS_KEY = "mac-storage-manager.scan-freshness.v1";
type Refresh = Pick<DiskScan, "scanId" | "roots" | "startedAtMs">;
export type ScanFreshness = { beforeMs: number; refreshes: Refresh[] };
const contains = (root: string, path: string) => path === root || path.startsWith(root === "/" ? root : `${root}/`);
const overlaps = (a: string, b: string) => contains(a, b) || contains(b, a);

export function readScanFreshness(value: string | null): ScanFreshness {
  try {
    const parsed = JSON.parse(value ?? "null") as ScanFreshness | null;
    if (parsed && Number.isFinite(parsed.beforeMs) && parsed.beforeMs >= 0 &&
        Array.isArray(parsed.refreshes) && parsed.refreshes.length <= 128 &&
        parsed.refreshes.every(item => item && typeof item.scanId === "string" &&
          Number.isFinite(item.startedAtMs) && item.startedAtMs >= 0 &&
          Array.isArray(item.roots) && item.roots.length > 0 &&
          item.roots.every(root => typeof root === "string" && root.startsWith("/")))) return parsed;
  } catch { /* A missing or invalid preference starts an empty ledger. */ }
  return { beforeMs: 0, refreshes: [] };
}

/** Invalidate when work starts, including work subsequently cancelled or failed.
 * Never graft independent subtree totals: hard-link attribution is scope-specific.
 */
export function recordScanRefresh(state: ScanFreshness, scan: Refresh): ScanFreshness {
  const refreshes = state.refreshes.filter(item => item.scanId !== scan.scanId &&
    !(item.startedAtMs <= scan.startedAtMs && item.roots.every(root => scan.roots.some(parent => contains(parent, root)))));
  refreshes.push({ scanId: scan.scanId, roots: [...scan.roots], startedAtMs: scan.startedAtMs });
  let beforeMs = state.beforeMs;
  // Bound persisted metadata. An overflow conservatively expires older snapshots.
  while (refreshes.length > 128) beforeMs = Math.max(beforeMs, refreshes.shift()!.startedAtMs);
  return { beforeMs, refreshes };
}

/** Check the displayed scope, so an unrelated sibling can still use its cache.
 * Start times matter: an older broad scan may finish after a focused refresh.
 */
export function needsScanRefresh(state: ScanFreshness, scan: Refresh, path?: string): boolean {
  if (state.beforeMs > 0 && scan.startedAtMs <= state.beforeMs) return true;
  const roots = path ? [path] : scan.roots;
  return state.refreshes.some(item => item.scanId !== scan.scanId &&
    item.startedAtMs >= scan.startedAtMs &&
    item.roots.some(refreshed => roots.some(root => overlaps(root, refreshed))));
}
