import type { DiskEntry, DiskScanComparison, DiskScanComparisonEntry } from "./disk-api";

export type ComparisonMetric = "growth" | "reduction";
export type ComparisonDetail = DiskScanComparisonEntry & { aggregate: boolean; residual: boolean };
export const comparisonWithin = (path: string, root: string) => path === root || path.startsWith(root === "/" ? root : `${root}/`);
const parentOf = (path: string) => path.slice(0, path.lastIndexOf("/")) || "/";
const clean = (path: string) => path === "/" ? path : path.replace(/\/+$/, "");
const nonnegative = (value: number) => Number.isFinite(value) ? Math.max(0, value) : 0;

/** Turn disjoint partitions into inclusive directory totals for the shared treemap. */
export function comparisonTreemap(comparison: DiskScanComparison, metric: ComparisonMetric, scope?: string, residualName = "Unitemized changes") {
  const roots = [...new Set((scope ? [scope] : comparison.roots).map(clean))].filter((root, _, all) => !all.some(other => other !== root && comparisonWithin(root, other)));
  const details = new Map<string, ComparisonDetail>();
  const entries = new Map<string, DiskEntry>();
  const positivePartitions = new Map<string, number>();
  const partitions: { partition: DiskScanComparisonEntry; root: string; directory: string; value: number }[] = [];
  const ensureDirectory = (path: string) => {
    if (!entries.has(path)) {
      const name = path.split("/").pop() || "/";
      entries.set(path, { path, name, parentPath: parentOf(path), kind: "directory", allocatedBytes: 0, logicalBytes: 0, fileCount: 0, directoryCount: 0, complete: true });
      details.set(path, { path, name, parentPath: parentOf(path), kind: "directory", beforeBytes: 0, afterBytes: 0, growthBytes: 0, reductionBytes: 0, netBytes: 0, detailLimited: true, aggregate: true, residual: false });
    }
  };
  for (const partition of comparison.entries) {
    const root = roots.find(candidate => comparisonWithin(partition.path, candidate));
    if (!root) continue;
    const value = nonnegative(metric === "growth" ? partition.growthBytes : partition.reductionBytes);
    // Keep both metrics in details, even when this side has no area.
    const isResidual = partition.kind === "directory";
    // A NUL cannot occur in a filesystem path, so residual IDs cannot collide.
    const id = isResidual ? `${partition.path === "/" ? "" : partition.path}/\0comparison-residual` : partition.path;
    entries.set(id, { path: id, name: isResidual ? residualName : partition.name, parentPath: isResidual ? partition.path : partition.parentPath, kind: isResidual ? "directory" : "file", allocatedBytes: value, logicalBytes: value, fileCount: 0, directoryCount: 0, complete: true });
    details.set(id, { ...partition, aggregate: false, residual: isResidual });
    const directory = isResidual ? partition.path : parentOf(partition.path);
    const relative = directory === root ? [] : directory.slice(root === "/" ? 1 : root.length + 1).split("/");
    ensureDirectory(root);
    // Preserve useful top-level navigation and the nearest four ancestors,
    // without materializing every level of each deeply nested retained path.
    for (let depth = 1; depth <= relative.length; depth++) {
      if (depth > 8 && depth <= relative.length - 4) continue;
      ensureDirectory(`${root === "/" ? "" : root}/${relative.slice(0, depth).join("/")}`);
    }
    partitions.push({ partition, root, directory, value });
  }
  // Another partition may have retained a directory skipped by this one's
  // depth budget. Include it in the totals too so every parent stays inclusive.
  for (const { partition, root, directory: start, value } of partitions) {
    let directory = start;
    while (comparisonWithin(directory, root)) {
      const entry = entries.get(directory);
      if (entry?.kind === "directory") {
        if (value > 0) positivePartitions.set(directory, (positivePartitions.get(directory) ?? 0) + 1);
        entry.allocatedBytes += value;
        entry.logicalBytes += value;
        const detail = details.get(directory)!;
        detail.beforeBytes += nonnegative(partition.beforeBytes);
        detail.afterBytes += nonnegative(partition.afterBytes);
        detail.growthBytes += nonnegative(partition.growthBytes);
        detail.reductionBytes += nonnegative(partition.reductionBytes);
        detail.netBytes += partition.netBytes;
      }
      if (directory === root || directory === "/") break;
      directory = parentOf(directory);
    }
  }
  // A directory already draws its inclusive total as a leaf when no other
  // positive partition is visible below it. Avoid repeating that same area in
  // an inner residual tile. Scope roots are excluded by buildTreemapTree, so
  // their residual keeps a drawable ID and takes the folder's name instead.
  for (const { partition, root, value } of partitions) {
    if (partition.kind !== "directory" || value <= 0 || positivePartitions.get(partition.path) !== 1) continue;
    const id = `${partition.path === "/" ? "" : partition.path}/\0comparison-residual`;
    if (partition.path === root) {
      entries.get(id)!.name = entries.get(root)!.name;
      entries.get(id)!.kind = "directory";
    } else {
      entries.delete(id);
      details.delete(id);
    }
  }
  return { roots, entries: [...entries.values()], details };
}
