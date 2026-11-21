import assert from "node:assert/strict";
import test from "node:test";
import { buildTreemapTree, layoutTreemap, squarify, treemapColorIndex } from "./treemap-layout.ts";

const row = (path, value, kind = "directory", complete = true) => ({
  path, name: path.split("/").pop(), parentPath: path.slice(0, path.lastIndexOf("/")) || "/",
  kind, allocatedBytes: value, logicalBytes: value, fileCount: 1, directoryCount: 0, complete,
});
const sum = (nodes) => nodes.reduce((total, node) => total + node.value, 0);
const near = (a, b) => assert.ok(Math.abs(a - b) < 0.00001, `${a} ≠ ${b}`);

test("parent totals, skipped inventory levels and duplicate large files are counted once", () => {
  const file = row("/scope/Library/cache/deep/big.bin", 25, "file");
  const { nodes: forest, mappedBytes, unmappedBytes } = buildTreemapTree([
    row("/scope", 120), row("/scope/Library", 100), row("/scope/Library/cache", 60),
    file, file, row("/scope/small.txt", 10, "file"), row("/outside", 999),
  ], ["/scope"]);
  assert.equal(mappedBytes, 110);
  assert.equal(unmappedBytes, 10);
  assert.deepEqual(forest.map((node) => node.id), ["/scope/Library", "/scope/small.txt"]);
  const library = forest.find((node) => node.id === "/scope/Library");
  assert.equal(library.value, 100);
  assert.equal(sum(library.children), 100);
  const cache = library.children.find((node) => node.id.endsWith("/cache"));
  assert.equal(sum(cache.children), 60);
  assert.equal(cache.children.filter((node) => node.entry?.path === file.path).length, 1);
  assert.equal(cache.children.find((node) => node.synthetic).value, 35);
});

test("overlapping roots and root-summary-only snapshots preserve measured bytes outside the map", () => {
  const overlapping = buildTreemapTree([row("/scope", 120), row("/scope/Library", 100)], ["/scope", "/scope/Library"]);
  assert.equal(overlapping.mappedBytes, 100);
  assert.equal(overlapping.unmappedBytes, 20);
  assert.equal(overlapping.nodes[0].id, "/scope/Library");
  const summaryOnly = buildTreemapTree([row("/scope", 60)], ["/scope"]);
  assert.deepEqual(summaryOnly, { nodes: [], mappedBytes: 0, unmappedBytes: 60 });
});

test("partial snapshot child totals ahead of parent stay nonnegative without parent-plus-child inflation", () => {
  const { nodes: forest, mappedBytes, unmappedBytes } = buildTreemapTree([row("/scope", 50, "directory", false), row("/scope/a", 30, "directory", false), row("/scope/a/file", 40, "file")], ["/scope"]);
  assert.equal(mappedBytes, 40);
  assert.equal(unmappedBytes, 10);
  assert.equal(forest.find((node) => node.id === "/scope/a").value, 40);
  assert.equal(forest.find((node) => node.id === "/scope/a").provisional, true);
});

test("child totals ahead of the scope total explicitly mark the chart provisional", () => {
  const { nodes: forest, mappedBytes, unmappedBytes } = buildTreemapTree([row("/scope", 80), row("/scope/a", 80), row("/scope/a/file", 100, "file")], ["/scope"]);
  assert.equal(mappedBytes, 100);
  assert.equal(unmappedBytes, 0);
  assert.equal(forest[0].provisional, true);
  assert.equal(forest[0].children[0].value, 100);
});

test("squarified rectangles conserve weighted area, stay within bounds and never overlap", () => {
  const nodes = [500, 250, 100, 80, 40, 25, 5].map((value) => ({ value }));
  const bounds = { x: 13, y: 9, width: 1000, height: 440 };
  const rectangles = squarify(nodes, bounds);
  for (const rect of rectangles) {
    near(rect.width * rect.height, rect.item.value / sum(nodes) * bounds.width * bounds.height);
    assert.ok(rect.width > 0 && rect.height > 0);
    assert.ok(rect.x >= bounds.x - 0.00001 && rect.y >= bounds.y - 0.00001);
    assert.ok(rect.x + rect.width <= bounds.x + bounds.width + 0.00001);
    assert.ok(rect.y + rect.height <= bounds.y + bounds.height + 0.00001);
  }
  near(rectangles.reduce((area, rect) => area + rect.width * rect.height, 0), 440000);
  for (let i = 0; i < rectangles.length; i++) for (let j = i + 1; j < rectangles.length; j++) {
    const a = rectangles[i], b = rectangles[j];
    const overlapX = Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x);
    const overlapY = Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y);
    assert.ok(overlapX <= 0.00001 || overlapY <= 0.00001);
  }
});

test("large inventories merge overflow with exact sums and obey the global nested tile limit", () => {
  const entries = [row("/scope", 2000)];
  for (let i = 0; i < 20; i++) {
    entries.push(row(`/scope/dir${i}`, 100));
    for (let j = 0; j < 100; j++) entries.push(row(`/scope/dir${i}/f${j}`, 1, "file"));
  }
  const { nodes: forest } = buildTreemapTree(entries, ["/scope"]);
  for (const maxTiles of [1, 5, 20, 100, 500]) {
    const tiles = layoutTreemap(forest, { x: 0, y: 0, width: 1400, height: 800 }, maxTiles);
    assert.ok(tiles.length <= maxTiles, `${tiles.length} exceeds ${maxTiles}`);
    assert.equal(sum(tiles.filter((tile) => tile.depth === 0).map((tile) => tile.node)), 2000);
    for (const tile of tiles.filter((item) => item.hasChildren)) {
      const children = tiles.filter((child) => child.depth === tile.depth + 1 && child.x >= tile.x && child.y >= tile.y && child.x < tile.x + tile.width && child.y < tile.y + tile.height);
      assert.equal(sum(children.map((child) => child.node)), tile.node.value);
    }
  }
});

test("a large folder still shows named third-level rows beside hundreds of small siblings", () => {
  const entries = [
    row("/scope", 90_499), row("/scope/projects", 90_000),
    row("/scope/projects/editor", 72_000), row("/scope/projects/editor/build-cache", 48_000),
    ...Array.from({ length: 499 }, (_, i) => row(`/scope/small-${i}`, 1)),
  ];
  const { nodes, mappedBytes } = buildTreemapTree(entries, ["/scope"]);
  const options = { reserveForDescendants: true };
  const tiles = layoutTreemap(nodes, { x: 0, y: 0, width: 1000, height: 360 }, 500, options);
  assert.ok(tiles.length <= 500);
  assert.equal(sum(tiles.filter((tile) => tile.depth === 0).map((tile) => tile.node)), mappedBytes);
  const buildCache = tiles.find((tile) => tile.node.name === "build-cache");
  assert.equal(buildCache?.depth, 2);
  assert.ok(buildCache.width >= 54 && buildCache.height >= 23, "the third-level label fits");
  assert.equal(tiles.find((tile) => tile.depth === 2 && tile.node.synthetic === "contents")?.node.value, 24_000);
  const compactTiles = layoutTreemap(nodes, { x: 0, y: 0, width: 85, height: 360 }, 500, options);
  assert.ok(compactTiles.every((tile) => tile.depth === 0));
});

test("a folder stays a real tile when the only child slot would be an aggregate", () => {
  const { nodes } = buildTreemapTree([
    row("/scope", 120), row("/scope/a", 100), row("/scope/a/payload", 10, "file"),
  ], ["/scope"]);
  const options = { reserveForDescendants: true };
  const bounds = { x: 0, y: 0, width: 1000, height: 360 };
  assert.equal(layoutTreemap(nodes, bounds, 2, options)[0].hasChildren, false);
  const expanded = layoutTreemap(nodes, bounds, 3, options);
  assert.equal(expanded[0].hasChildren, true);
  assert.equal(expanded[1].node.synthetic, "contents");
  assert.equal(expanded[2].node.entry.path, "/scope/a/payload");
});

test("zero-size scans have no rectangles and branch colors are stable across ordering", () => {
  assert.deepEqual(buildTreemapTree([row("/scope", 0), row("/scope/empty", 0)], ["/scope"]), { nodes: [], mappedBytes: 0, unmappedBytes: 0 });
  assert.deepEqual(squarify([{ value: 0 }], { x: 0, y: 0, width: 100, height: 100 }), []);
  assert.equal(treemapColorIndex("/scope/Library", 8), treemapColorIndex("/scope/Library", 8));
  assert.ok(treemapColorIndex("/scope/Library", 8) < 8);
});

test("nested residuals and budget groups retain a route to their concrete contents", () => {
  const { nodes: forest } = buildTreemapTree([row('/scope', 120), row('/scope/a', 100), row('/scope/a/file', 20, 'file')], ['/scope']);
  const childRemainder = forest.find(node => node.entry?.name === 'a').children.find(node => node.synthetic);
  assert.equal(childRemainder.inspectPath, '/scope/a');
  const siblings = Array.from({length: 80}, (_, i) => row(`/scope/d${i}`, 10));
  const tiles = layoutTreemap(buildTreemapTree(siblings, ['/scope']).nodes, {x:0,y:0,width:800,height:500}, 3);
  const grouped = tiles.find(t => t.node.synthetic === 'group').node;
  assert.equal(grouped.inspectPath, '/scope');
  assert.equal(grouped.groupedNodes.length, 78);
  assert.equal(sum(grouped.groupedNodes), grouped.value);
  assert.ok(grouped.groupedNodes.every(n => n.entry.path.startsWith('/scope/')));
});

test("a folder without weighted child rows remains a single real tile", () => {
  const { nodes } = buildTreemapTree([
    row("/scope", 120), row("/scope/photos", 100), row("/scope/photos/empty", 0),
  ], ["/scope"]);
  const tiles = layoutTreemap(nodes, { x: 0, y: 0, width: 1000, height: 500 });
  assert.equal(tiles.length, 1);
  assert.equal(tiles[0].node.entry.path, "/scope/photos");
  assert.equal(tiles[0].hasChildren, false);
});

test("a large remainder never consumes the active folder’s canvas", () => {
  const scope = "/scope/attachments";
  const { nodes, mappedBytes, unmappedBytes } = buildTreemapTree([
    row(scope, 11700), row(`${scope}/photos`, 900), row(`${scope}/notes.zip`, 400, "file"),
  ], [scope]);
  assert.equal(mappedBytes, 1300);
  assert.equal(unmappedBytes, 10400);
  const tiles = layoutTreemap(nodes, { x: 0, y: 0, width: 1000, height: 500 });
  assert.deepEqual(tiles.map(({ node }) => node.entry?.path), [`${scope}/photos`, `${scope}/notes.zip`]);
  near(tiles[0].width * tiles[0].height / 500000, 900 / 1300);
  near(tiles[1].width * tiles[1].height / 500000, 400 / 1300);
});
