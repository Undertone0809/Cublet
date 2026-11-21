import type { DiskEntry } from "./disk-api";

export type TreemapNode = {
  id: string;
  name: string;
  value: number;
  entry?: DiskEntry;
  children: TreemapNode[];
  branch: string;
  synthetic?: "contents" | "group";
  groupedCount?: number;
  inspectPath?: string;
  groupedNodes?: TreemapNode[];
  provisional?: boolean;
};
export type TreemapRect = { x: number; y: number; width: number; height: number };
export type TreemapTile = TreemapRect & {
  node: TreemapNode;
  depth: number;
  hasChildren: boolean;
};
export type TreemapTree = {
  nodes: TreemapNode[];
  mappedBytes: number;
  unmappedBytes: number;
};
const bytes = (value: number) => Number.isFinite(value) ? Math.max(0, value) : 0;
const cleanPath = (path: string) => path === "/" ? path : path.replace(/\/+$/, "");
const within = (path: string, root: string) => path === root || path.startsWith(root === "/" ? root : `${root}/`);
const descending = (a: TreemapNode, b: TreemapNode) => b.value - a.value || a.id.localeCompare(b.id);

/** Directory totals include their descendants. Only leaf partitions are added. */
export function buildTreemapTree(entries: DiskEntry[], roots: string[]): TreemapTree {
  const rootPaths = [...new Set(roots.map(cleanPath))].filter((root, _, all) =>
    !all.some((other) => other !== root && within(root, other)),
  );
  const byPath = new Map<string, DiskEntry>();
  for (const entry of entries) {
    const path = cleanPath(entry.path);
    if (rootPaths.some((root) => within(path, root))) byPath.set(path, entry);
  }
  const nodesByPath = new Map<string, TreemapNode>();
  for (const [path, entry] of byPath) {
    if (!rootPaths.includes(path)) nodesByPath.set(path, {
      id: path, name: entry.name, value: bytes(entry.allocatedBytes), entry, children: [], branch: path,
    });
  }
  const forest: TreemapNode[] = [];
  for (const [path, node] of nodesByPath) {
    // Large-file rows can skip inventory levels; attach to their nearest known
    // ancestor instead of drawing the same bytes beside that ancestor.
    let parent = cleanPath(node.entry!.parentPath);
    let ancestor: TreemapNode | undefined;
    while (parent && !rootPaths.includes(parent)) {
      const candidate = nodesByPath.get(parent);
      if (candidate && candidate.entry?.kind !== "file") { ancestor = candidate; break; }
      const next = parent.slice(0, parent.lastIndexOf("/")) || "/";
      if (next === parent) break;
      parent = next;
    }
    if (ancestor && ancestor.id !== path) ancestor.children.push(node);
    else forest.push(node);
  }
  const finish = (node: TreemapNode, branch: string) => {
    node.branch = branch;
    for (const child of node.children) finish(child, branch);
    node.children = node.children.filter((child) => child.value > 0);
    const childBytes = node.children.reduce((sum, child) => sum + child.value, 0);
    // Snapshots may arrive while a parent total is still catching up. Using max
    // keeps geometry nonnegative and never adds parent + child totals together.
    node.provisional = childBytes > node.value || node.children.some((child) => child.provisional);
    node.value = Math.max(node.value, childBytes);
    if (node.children.length && node.value > childBytes) node.children.push({
      id: `${node.id}::contents`, name: "", value: node.value - childBytes,
      children: [], branch, synthetic: "contents", entry: undefined, inspectPath: node.id,
    });
    node.children.sort(descending);
  };
  for (const node of forest) finish(node, node.id);
  // The canvas represents the scope itself. Track measured allocation missing
  // from the input rows separately so it never competes with actual children.
  let unmappedBytes = 0;
  for (const root of rootPaths) {
    const summary = byPath.get(root);
    if (!summary) continue;
    const scopedNodes = forest.filter((node) => within(node.id, root));
    const covered = scopedNodes.reduce((sum, node) => sum + node.value, 0);
    const difference = bytes(summary.allocatedBytes) - covered;
    if (difference < 0) for (const node of scopedNodes) node.provisional = true;
    else unmappedBytes += difference;
  }
  const nodes = forest.filter((node) => node.value > 0).sort(descending);
  return { nodes, mappedBytes: nodes.reduce((sum, node) => sum + node.value, 0), unmappedBytes };
}

/** Bruls/Huizing/van Wijk squarification, preserving the full input weight. */
export function squarify<T extends { value: number }>(items: T[], bounds: TreemapRect): (TreemapRect & { item: T })[] {
  const total = items.reduce((sum, item) => sum + bytes(item.value), 0);
  if (total <= 0 || bounds.width <= 0 || bounds.height <= 0) return [];
  const scale = bounds.width * bounds.height / total;
  const pending = items.filter((item) => item.value > 0).map((item) => ({ item, area: item.value * scale })).sort((a, b) => b.area - a.area);
  const result: (TreemapRect & { item: T })[] = [];
  let remaining = { ...bounds };
  let row: typeof pending = [];
  const worst = (values: typeof pending, side: number) => {
    if (!values.length || side <= 0) return Infinity;
    const sum = values.reduce((s, v) => s + v.area, 0);
    const min = Math.min(...values.map((v) => v.area));
    const max = Math.max(...values.map((v) => v.area));
    return Math.max(side * side * max / (sum * sum), sum * sum / (side * side * min));
  };
  const place = (values: typeof pending) => {
    const sum = values.reduce((s, v) => s + v.area, 0);
    const vertical = remaining.width >= remaining.height;
    const side = vertical ? remaining.height : remaining.width;
    const thickness = side > 0 ? Math.min(vertical ? remaining.width : remaining.height, sum / side) : 0;
    let cursor = vertical ? remaining.y : remaining.x;
    values.forEach((value, index) => {
      const length = index === values.length - 1
        ? (vertical ? remaining.y + remaining.height : remaining.x + remaining.width) - cursor
        : thickness > 0 ? value.area / thickness : 0;
      result.push({ item: value.item,
        x: vertical ? remaining.x : cursor, y: vertical ? cursor : remaining.y,
        width: vertical ? thickness : length, height: vertical ? length : thickness,
      });
      cursor += length;
    });
    if (vertical) { remaining.x += thickness; remaining.width = Math.max(0, remaining.width - thickness); }
    else { remaining.y += thickness; remaining.height = Math.max(0, remaining.height - thickness); }
  };
  for (const item of pending) {
    const side = Math.min(remaining.width, remaining.height);
    if (row.length && worst([...row, item], side) > worst(row, side)) { place(row); row = []; }
    row.push(item);
  }
  if (row.length) place(row);
  return result;
}

function limitSiblings(nodes: TreemapNode[], budget: number): TreemapNode[] {
  if (nodes.length <= budget) return nodes;
  const kept = nodes.slice(0, Math.max(0, budget - 1));
  const omitted = nodes.slice(kept.length);
  const parentOf = (node: TreemapNode) => node.entry?.parentPath ?? node.inspectPath;
  const firstParent = parentOf(omitted[0]);
  const commonParent = omitted.every((node) => parentOf(node) === firstParent) ? firstParent : undefined;
  return [...kept, {
    id: `${omitted[0].id}::group`, name: "", synthetic: "group",
    value: omitted.reduce((sum, node) => sum + node.value, 0), children: [],
    branch: omitted.every((node) => node.branch === omitted[0].branch) ? omitted[0].branch : "other",
    groupedCount: omitted.length,
    inspectPath: commonParent,
    groupedNodes: omitted,
    provisional: omitted.some((node) => node.provisional),
  }];
}

/** Budget includes parent headers, so even pathological inventories stay bounded. */
export function layoutTreemap(
  nodes: TreemapNode[],
  bounds: TreemapRect,
  maxTiles = 500,
  { reserveForDescendants = false } = {},
): TreemapTile[] {
  const tiles: TreemapTile[] = [];
  const visit = (siblings: TreemapNode[], rect: TreemapRect, depth: number, budget: number) => {
    const fitsChildren = (box: TreemapRect) => depth < 5 && box.width >= 88 && box.height >= 62;
    let layouts = squarify(limitSiblings(siblings, Math.max(1, budget)), rect);
    const reserve = Math.floor(budget / 4);
    if (reserveForDescendants && reserve > 0 && layouts.length > budget - reserve &&
        layouts.some((box) => fitsChildren(box) && box.item.children.some((child) => child.entry))) {
      layouts = squarify(limitSiblings(siblings, budget - reserve), rect);
    }
    const expandable = layouts.map((box) => {
      const { children } = box.item;
      if (!children.length) return { box, minimum: Infinity, weight: 0 };
      if (!reserveForDescendants) return { box, minimum: 1, weight: box.item.value };
      const firstRealChild = children.findIndex((child) => child.entry);
      if (!fitsChildren(box) || firstRealChild < 0) return { box, minimum: Infinity, weight: 0 };
      // When siblings are grouped, the last slot describes the omitted rows.
      const minimum = Math.min(children.length, firstRealChild + 2);
      return { box, minimum, weight: box.item.value };
    });
    let extra = budget - layouts.length;
    let remainingWeight = expandable.reduce((sum, { weight }) => sum + weight, 0);
    for (const { box: { item: node, ...box }, minimum, weight } of expandable) {
      const share = weight > 0 && remainingWeight > 0
        ? Math.min(extra, Math.max(0, Math.floor(extra * weight / remainingWeight))) : 0;
      const hasChildren = fitsChildren(box) && share >= minimum;
      tiles.push({ ...box, node, depth, hasChildren });
      if (hasChildren) {
        const before = tiles.length;
        visit(node.children, { x: box.x + 3, y: box.y + 25, width: box.width - 6, height: box.height - 28 }, depth + 1, share);
        extra -= reserveForDescendants ? tiles.length - before : share;
      }
      remainingWeight -= weight;
    }
  };
  if (maxTiles > 0) visit(nodes, bounds, 0, Math.floor(maxTiles));
  return tiles;
}

export function treemapColorIndex(branch: string, count: number): number {
  let hash = 2166136261;
  for (let i = 0; i < branch.length; i++) hash = Math.imul(hash ^ branch.charCodeAt(i), 16777619);
  return (hash >>> 0) % count;
}
