import assert from "node:assert/strict";
import test from "node:test";
import { comparisonTreemap } from "./scan-comparison.ts";
import { buildTreemapTree } from "./treemap-layout.ts";

const partition = (path, beforeBytes, afterBytes, kind = "directory") => ({ path, name: path.split("/").pop(), parentPath: path.slice(0, path.lastIndexOf("/")) || "/", kind, beforeBytes, afterBytes, growthBytes: Math.max(0, afterBytes - beforeBytes), reductionBytes: Math.max(0, beforeBytes - afterBytes), netBytes: afterBytes - beforeBytes, detailLimited: kind === "directory" });
const comparison = (entries, roots = ["/scope"]) => ({ status: "ready", roots, entries, growthBytes: entries.reduce((sum, row) => sum + row.growthBytes, 0), reductionBytes: entries.reduce((sum, row) => sum + row.reductionBytes, 0) });

test("disjoint residual and descendant growth add without cancellation or double counting", () => {
  const data = comparison([partition("/scope/a", 50, 80), partition("/scope/a/deep/file", 10, 110, "file"), partition("/scope/a/removed", 80, 0)]);
  const growth = comparisonTreemap(data, "growth");
  const growthTree = buildTreemapTree(growth.entries, growth.roots);
  assert.equal(growthTree.mappedBytes, 130);
  assert.equal(growthTree.unmappedBytes, 0);
  assert.equal(growthTree.nodes[0].value, 130);
  assert.equal(growth.details.get("/scope/a").growthBytes, 130);
  assert.equal(growth.details.get("/scope/a").reductionBytes, 80);
  assert.equal(growth.details.get("/scope/a").netBytes, 50);
  const residual = growth.entries.find(row => row.path.includes("\0"));
  assert.equal(residual.allocatedBytes, 30);
  assert.equal(growth.details.get(residual.path).path, "/scope/a");
  const reductions = comparisonTreemap(data, "reduction");
  assert.equal(buildTreemapTree(reductions.entries, reductions.roots).mappedBytes, 80);
});

test("scope-root residual remains drawable and never becomes unmapped coverage", () => {
  const data = comparison([partition("/scope", 100, 140)]);
  const model = comparisonTreemap(data, "growth");
  const tree = buildTreemapTree(model.entries, model.roots);
  assert.equal(tree.mappedBytes, 40);
  assert.equal(tree.unmappedBytes, 0);
  assert.equal(tree.nodes.length, 1);
  assert.equal(tree.nodes[0].name, "scope");
  assert.equal(tree.nodes[0].entry.kind, "directory");
  assert.equal(tree.nodes[0].children.length, 0);
  assert.equal(model.details.get(tree.nodes[0].id).residual, true);
  assert.equal(model.details.get(tree.nodes[0].id).path, "/scope");
});

test("residual-only growth draws a named directory leaf with historical navigation", () => {
  const data = comparison([partition("/scope/cache", 100, 140), partition("/scope/cache/removed", 60, 0), partition("/scope/cache/unchanged", 10, 10, "file")]);
  const model = comparisonTreemap(data, "growth", undefined, "Other growth");
  const tree = buildTreemapTree(model.entries, model.roots);
  const folder = tree.nodes[0];
  assert.equal(tree.mappedBytes, 40);
  assert.equal(tree.unmappedBytes, 0);
  assert.equal(tree.nodes.length, 1);
  assert.equal(folder.id, "/scope/cache");
  assert.equal(folder.name, "cache");
  assert.equal(folder.entry.kind, "directory");
  assert.equal(folder.children.length, 0);
  assert.equal(model.entries.some(row => row.path === "/scope/cache/\0comparison-residual"), false);
  assert.deepEqual(Object.fromEntries(["beforeBytes", "afterBytes", "growthBytes", "reductionBytes", "netBytes"].map(key => [key, model.details.get(folder.id)[key]])), {
    beforeBytes: 170, afterBytes: 150, growthBytes: 40, reductionBytes: 60, netBytes: -20,
  });
  assert.equal(model.details.get(folder.id).path, "/scope/cache");
  assert.equal(model.details.get(folder.id).aggregate, true);
  assert.equal(model.details.get(folder.id).residual, false);
  assert.ok(model.entries.every(row => row.fileCount >= 0 && row.directoryCount >= 0));
});

test("mixed grown descendants retain their separately named residual area", () => {
  const data = comparison([partition("/scope/cache", 100, 140), partition("/scope/cache/deep/file", 5, 15, "file")]);
  const model = comparisonTreemap(data, "growth", undefined, "其他增长");
  const tree = buildTreemapTree(model.entries, model.roots);
  const folder = tree.nodes[0];
  const residual = folder.children.find(node => node.id.includes("\0"));
  assert.equal(tree.mappedBytes, 50);
  assert.equal(tree.unmappedBytes, 0);
  assert.equal(folder.value, 50);
  assert.equal(folder.children.length, 2);
  assert.equal(residual.name, "其他增长");
  assert.equal(residual.value, 40);
  assert.equal(residual.entry.kind, "directory", "An aggregate represents a folder, never a file to reveal");
  assert.equal(model.details.get(residual.id).growthBytes, 40);
  assert.equal(model.details.get("/scope/cache").growthBytes, 50);
});

test("scope residual uses the folder name only when it is the sole positive partition", () => {
  const data = comparison([partition("/scope/cache", 100, 140), partition("/scope/cache/file", 10, 20, "file")]);
  const mixed = comparisonTreemap(data, "growth", "/scope/cache", "Other growth");
  const mixedTree = buildTreemapTree(mixed.entries, mixed.roots);
  assert.equal(mixedTree.mappedBytes, 50);
  assert.equal(mixedTree.unmappedBytes, 0);
  assert.equal(mixedTree.nodes.find(node => node.id.includes("\0")).name, "Other growth");
  const sole = comparisonTreemap(comparison([partition("/scope/cache", 100, 140)]), "growth", "/scope/cache", "Other growth");
  const soleTree = buildTreemapTree(sole.entries, sole.roots);
  assert.equal(soleTree.nodes[0].name, "cache");
  assert.equal(soleTree.nodes[0].value, 40);
  assert.equal(sole.details.get(soleTree.nodes[0].id).path, "/scope/cache");
});

test("scoped drill excludes siblings and overlapping roots conserve area", () => {
  const data = comparison([partition("/scope/a/file", 10, 20, "file"), partition("/scope/ab/file", 0, 90, "file")], ["/scope", "/scope/a"]);
  const all = comparisonTreemap(data, "growth");
  assert.equal(buildTreemapTree(all.entries, all.roots).mappedBytes, 100);
  const scoped = comparisonTreemap(data, "growth", "/scope/a");
  assert.equal(buildTreemapTree(scoped.entries, scoped.roots).mappedBytes, 10);
  assert.equal(scoped.details.has("/scope/ab/file"), false);
});

test("filesystem root and missing inventory levels preserve residual plus file area", () => {
  const data = comparison([partition("/", 10, 30), partition("/one/two/file", 0, 70, "file")], ["/"]);
  const model = comparisonTreemap(data, "growth");
  const tree = buildTreemapTree(model.entries, model.roots);
  assert.equal(tree.mappedBytes, 90);
  assert.equal(tree.unmappedBytes, 0);
  assert.equal(model.details.get("/one").growthBytes, 70);
});

test("deep inventories bound synthesized ancestors while preserving every partition", () => {
  const entries = Array.from({ length: 1024 }, (_, index) => partition(`/scope/branch${index}/${Array.from({ length: 100 }, (_, depth) => `d${depth}`).join("/")}/file`, 10, 13, "file"));
  const model = comparisonTreemap(comparison(entries), "growth");
  assert.ok(model.entries.length <= 1 + 13 * entries.length);
  const tree = buildTreemapTree(model.entries, model.roots);
  assert.equal(tree.mappedBytes, 3072);
  assert.equal(tree.unmappedBytes, 0);
  assert.equal(model.details.get("/scope").beforeBytes, 10240);
  assert.equal(model.details.get("/scope").afterBytes, 13312);
});

test("ancestors retained by another partition receive all descendant metrics", () => {
  const parts = Array.from({ length: 30 }, (_, depth) => `d${depth}`);
  const deep = `/scope/${parts.join("/")}`;
  const shared = `/scope/${parts.slice(0, 15).join("/")}`;
  const model = comparisonTreemap(comparison([partition(deep, 10, 30), partition(shared, 7, 17)]), "growth");
  assert.equal(model.details.get(shared).growthBytes, 30);
  assert.equal(model.details.get(shared).beforeBytes, 17);
  assert.equal(model.details.get(shared).afterBytes, 47);
  const tree = buildTreemapTree(model.entries, model.roots);
  assert.equal(tree.mappedBytes, 30);
  assert.equal(tree.unmappedBytes, 0);
  assert.equal(tree.nodes.some(node => node.provisional), false);
});
