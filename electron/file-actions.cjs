const fs = require("node:fs/promises");
const path = require("node:path");
const os = require("node:os");
const { randomUUID } = require("node:crypto");
// Selection metadata is live filesystem state; scan rows never authorize a move.
function createEntryActions({ shell, now = Date.now, planTtlMs = 120000, fileSystem = fs, home = os.homedir() }) {
  const trashPath = path.join(home, ".Trash");
  const plans = new Map();
  let moving = false;
  const kindOf = s => s.isSymbolicLink() ? "symlink" : s.isFile() ? "file" : s.isDirectory() ? "directory" : "other";
  // Contents, dates, link count and allocation may change while confirmation is
  // open (for example, an active cache). Only replacement invalidates identity.
  const identity = s => ({ dev: s.dev, ino: s.ino, birthtimeMs: s.birthtimeMs, kind: kindOf(s) });
  function validPath(target) {
    if (typeof target !== "string" || !path.isAbsolute(target) || target.includes("\0"))
      throw new Error("请选择有效的绝对路径 / Select a valid absolute path");
    return target;
  }
  function permissionError(error) {
    if (["EACCES", "EPERM"].includes(error?.code) || /permission denied|operation not permitted|access denied|not authorized/i.test(error?.message || "")) {
      const normalized = new Error(`permission-required: macOS 拒绝访问，请检查磁盘访问权限 / macOS denied access; check disk permissions (${error.message})`);
      normalized.code = error.code || "EACCES";
      return normalized;
    }
    return error;
  }
  async function inspect(target) {
    validPath(target);
    // Resolve only the parent. Following the selected symlink would move its
    // destination instead of the link the user selected. Native Trash receives
    // this resolved path so a subsequently retargeted parent alias is not used.
    let parent = await fileSystem.realpath(path.dirname(target));
    let name = path.basename(target);
    // A root or explicit dot component represents its resolved directory.
    if (!name || name === "." || name === "..") {
      const resolved = await fileSystem.realpath(target);
      parent = path.dirname(resolved);
      name = path.basename(resolved);
    }
    const resolvedPath = path.join(parent, name);
    const ancestors = [];
    let cursor = path.parse(parent).root;
    ancestors.push({ path: cursor, ...identity(await fileSystem.lstat(cursor)) });
    for (const part of parent.slice(cursor.length).split(path.sep).filter(Boolean)) {
      cursor = path.join(cursor, part);
      const stat = await fileSystem.lstat(cursor);
      // realpath returned canonical ancestors. If one changed into a link while
      // collecting identities, retry through a fresh user preview.
      if (!stat.isDirectory()) throw new Error("路径已变化，请重新确认 / Path changed; confirm again");
      ancestors.push({ path: cursor, ...identity(stat) });
    }
    const stat = await fileSystem.lstat(resolvedPath);
    return { path: target, resolvedPath, name: path.basename(target) || target, kind: kindOf(stat), ancestors, ...identity(stat),
      createdMs: Number.isFinite(stat.birthtimeMs) && stat.birthtimeMs > 0 ? stat.birthtimeMs : null,
      modifiedMs: Number.isFinite(stat.mtimeMs) ? stat.mtimeMs : null };
  }
  const sameEntry = (first, second) => JSON.stringify({ path: first.resolvedPath, ...identityFields(first), ancestors: first.ancestors })
    === JSON.stringify({ path: second.resolvedPath, ...identityFields(second), ancestors: second.ancestors });
  const identityFields = item => ({ dev: item.dev, ino: item.ino, birthtimeMs: item.birthtimeMs, kind: item.kind });
  async function isTrashRoot(item) {
    const root = await fileSystem.realpath(home);
    const canonicalTrash = path.join(root, ".Trash");
    if (item.resolvedPath !== canonicalTrash) return false;
    if (item.kind !== "directory") throw new Error("废纸篓路径已变化，请检查 Finder / Trash path changed; check Finder");
    return true;
  }
  async function trashEntries(item) {
    const entries = [];
    for (const name of await fileSystem.readdir(item.resolvedPath)) {
      const target = path.join(item.resolvedPath, name);
      entries.push({ path: target, ...identity(await fileSystem.lstat(target)) });
    }
    return entries;
  }
  async function details(target) {
    try {
      const item = await inspect(target);
      return { path: item.path, name: item.name, kind: item.kind, createdMs: item.createdMs, modifiedMs: item.modifiedMs, canTrash: true };
    } catch (error) { throw permissionError(error); }
  }
  async function preview(target) {
    try {
      const item = await inspect(target);
      for (const [key, plan] of plans) if (plan.expires <= now()) plans.delete(key);
      while (plans.size >= 24) plans.delete(plans.keys().next().value);
      const planId = randomUUID();
      const emptyTrash = await isTrashRoot(item);
      const entries = emptyTrash ? await trashEntries(item) : undefined;
      plans.set(planId, { item, entries, expires: now() + planTtlMs });
      return { planId, path: item.path, name: item.name, kind: item.kind, ...(emptyTrash ? { action: "empty-trash", itemCount: entries.length } : {}) };
    } catch (error) { throw permissionError(error); }
  }
  async function execute(planId, confirmed, action = "move-to-trash") {
    if (confirmed !== true) throw new Error("请再次确认移到废纸篓 / Confirm moving this item to Trash");
    if (moving) throw new Error("正在移动其他项目，请稍后重试 / Another Trash move is in progress");
    const plan = plans.get(planId);
    plans.delete(planId); // An execution attempt consumes its authorization, even on failure.
    if (!plan || plan.expires <= now()) throw new Error("预览已过期，请重新确认 / Preview expired; confirm again");
    moving = true;
    try {
      const current = await inspect(plan.item.path);
      if (!sameEntry(current, plan.item)) throw new Error("项目已变化，请重新确认 / Item changed; confirm again");
      const emptyTrash = await isTrashRoot(current);
      if (emptyTrash !== !!plan.entries || action !== (emptyTrash ? "empty-trash" : "move-to-trash"))
        throw new Error("操作已变化，请重新确认 / Action changed; confirm again");
      if (emptyTrash) {
        // Validate the complete snapshot before the first irreversible deletion.
        // Only direct entries in this preview are authorized; new arrivals remain.
        for (const entry of plan.entries) {
          const live = identity(await fileSystem.lstat(entry.path));
          if (JSON.stringify(live) !== JSON.stringify(identityFields(entry)))
            throw new Error("废纸篓内容已变化，请重新确认 / Trash contents changed; confirm again");
        }
        const failures = [];
        let removedCount = 0;
        for (const entry of plan.entries) {
          try {
            const root = await inspect(plan.item.path);
            if (!sameEntry(root, plan.item)) throw new Error("Trash directory changed");
            const live = identity(await fileSystem.lstat(entry.path));
            if (JSON.stringify(live) !== JSON.stringify(identityFields(entry))) throw new Error("Trash item changed");
            // fs.rm unlinks symbolic links, including nested links; it does not
            // follow their destinations. Never remove the Trash root itself.
            await fileSystem.rm(entry.path, { recursive: true, force: false });
            removedCount++;
          } catch (error) { failures.push({ name: path.basename(entry.path), message: permissionError(error).message }); }
        }
        let remainingCount;
        try { remainingCount = (await fileSystem.readdir(current.resolvedPath)).length; }
        catch (error) { failures.push({ name: "", message: permissionError(error).message }); }
        return { action: "empty-trash", movedPath: current.path, resolvedPath: current.resolvedPath, removedCount, remainingCount, failures };
      }
      await shell.trashItem(current.resolvedPath);
      return { movedPath: current.path, resolvedPath: current.resolvedPath };
    } catch (error) { throw permissionError(error); }
    finally { moving = false; }
  }
  return { details, preview, execute, trashPath };
}
module.exports = { createEntryActions };
