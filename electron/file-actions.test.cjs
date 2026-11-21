const { test } = require("node:test");
const assert = require("node:assert/strict");
const os = require("node:os");
const path = require("node:path");
const fs = require("node:fs/promises");
const { createEntryActions } = require("./file-actions.cjs");

async function withEntryFixture(run) {
  const base = await fs.mkdtemp(path.join(await fs.realpath(os.tmpdir()), "msm-entry-test-"));
  const home = path.join(base, "home");
  const moved = [];
  await fs.mkdir(home);
  const shell = { trashItem: async target => { moved.push(target); } };
  try { await run({ base, home, moved, shell, actions: createEntryActions({ home, shell }) }); }
  finally { await fs.rm(base, { recursive: true, force: true }); }
}

test("Library, hidden paths, Home folders, applications and outside-Home entries are user-authorized", async () => {
  await withEntryFixture(async ({ base, home, actions, moved }) => {
    for (const target of [home, ...["Library", "Nested/.Trash", ".codex", "Downloads", "Applications", "Projects/demo.app", "Projects/.private"]
      .map(relative => path.join(home, relative)), path.join(base, "outside-home")]) {
      await fs.mkdir(target, { recursive: true });
      const details = await actions.details(target);
      assert.equal(details.kind, "directory");
      assert.ok(Number.isFinite(details.createdMs));
      assert.ok(Number.isFinite(details.modifiedMs));
      assert.equal(details.canTrash, true, target);
      const plan = await actions.preview(target);
      await actions.execute(plan.planId, true);
      assert.equal(moved.at(-1), target);
    }
    for (const target of ["relative/file", "", null, `${home}/bad\0path`]) await assert.rejects(actions.preview(target), /absolute path/);
    await assert.rejects(actions.preview(path.join(home, "missing")), { code: "ENOENT" });
  });
});

test("opaque plans move regular files and directories once and require explicit confirmation", async () => {
  await withEntryFixture(async ({ home, actions, moved }) => {
    const dir = path.join(home, "Documents", "Project");
    await fs.mkdir(dir, { recursive: true });
    const file = path.join(dir, "sample.txt");
    await fs.writeFile(file, "fixture");
    assert.equal((await actions.details(file)).canTrash, true);
    const filePlan = await actions.preview(file);
    assert.deepEqual(Object.keys(filePlan).sort(), ["kind", "name", "path", "planId"]);
    await assert.rejects(actions.execute(filePlan.planId, false), /Confirm/);
    assert.deepEqual(await actions.execute(filePlan.planId, true), { movedPath: file, resolvedPath: file });
    await assert.rejects(actions.execute(filePlan.planId, true), /expired/);
    const dirPlan = await actions.preview(dir);
    assert.equal(dirPlan.kind, "directory");
    await actions.execute(dirPlan.planId, true);
    assert.deepEqual(moved, [file, dir]);
  });
});

test("content and directory edits remain authorized while identity replacement invalidates previews", async () => {
  await withEntryFixture(async ({ home, actions, moved }) => {
    const file = path.join(home, "sample.txt");
    await fs.writeFile(file, "one");
    const changed = await actions.preview(file);
    await fs.writeFile(file, "changed content");
    await actions.execute(changed.planId, true);
    const replaced = await actions.preview(file);
    await fs.rename(file, `${file}.old`);
    await fs.writeFile(file, "changed content");
    await assert.rejects(actions.execute(replaced.planId, true), /changed/);
    const dir = path.join(home, "folder");
    await fs.mkdir(dir);
    const directory = await actions.preview(dir);
    await fs.writeFile(path.join(dir, "added"), "new");
    await fs.utimes(dir, new Date(), new Date(Date.now() + 2000));
    await actions.execute(directory.planId, true);
    assert.deepEqual(moved, [file, dir]);
  });
});

test("native Trash receives the symlink entry itself and hardlinked entries without changing their peers", async () => {
  await withEntryFixture(async ({ base, home }) => {
    const original = path.join(home, "original");
    await fs.mkdir(original);
    const source = path.join(original, "file");
    await fs.writeFile(source, "fixture");
    const trash = path.join(base, "fake-trash");
    await fs.mkdir(trash);
    const received = [];
    const actions = createEntryActions({ shell: { trashItem: async target => {
      received.push(target);
      await fs.rename(target, path.join(trash, path.basename(target)));
    } } });
    const alias = path.join(home, "alias");
    await fs.symlink(source, alias);
    assert.equal((await actions.details(alias)).kind, "symlink");
    const linkPlan = await actions.preview(alias);
    await actions.execute(linkPlan.planId, true);
    assert.equal(await fs.readFile(source, "utf8"), "fixture");
    assert.equal((await fs.lstat(path.join(trash, "alias"))).isSymbolicLink(), true);
    await assert.rejects(fs.lstat(alias), { code: "ENOENT" });
    const hardlink = path.join(original, "hardlink");
    await fs.link(source, hardlink);
    const hardlinkPlan = await actions.preview(hardlink);
    await actions.execute(hardlinkPlan.planId, true);
    assert.equal(await fs.readFile(source, "utf8"), "fixture");
    assert.equal((await fs.lstat(path.join(trash, "hardlink"))).ino, (await fs.lstat(source)).ino);
    assert.deepEqual(received, [alias, hardlink]);
    const broken = path.join(home, "broken-link");
    await fs.symlink(path.join(base, "absent-destination"), broken);
    const brokenPlan = await actions.preview(broken);
    await actions.execute(brokenPlan.planId, true);
    assert.equal((await fs.lstat(path.join(trash, "broken-link"))).isSymbolicLink(), true);
  });
});

test("parent symlinks are usable, resolve the selected entry, and cannot redirect a confirmed preview", async () => {
  await withEntryFixture(async ({ base, home, actions, moved }) => {
    const original = path.join(base, "actual-directory");
    const other = path.join(base, "other-directory");
    await fs.mkdir(original);
    await fs.mkdir(other);
    await fs.writeFile(path.join(original, "file"), "fixture");
    await fs.writeFile(path.join(other, "file"), "unrelated");
    const alias = path.join(home, "alias");
    await fs.symlink(original, alias);
    const selected = path.join(alias, "file");
    const usable = await actions.preview(selected);
    assert.deepEqual(await actions.execute(usable.planId, true), { movedPath: selected, resolvedPath: path.join(original, "file") });
    assert.deepEqual(moved, [path.join(original, "file")]);
    const stale = await actions.preview(selected);
    await fs.unlink(alias);
    await fs.symlink(other, alias);
    await assert.rejects(actions.execute(stale.planId, true), /changed/);
    assert.equal(moved.length, 1);
  });
});

test("changed parent identity cannot transfer a preview to a different directory tree", async () => {
  await withEntryFixture(async ({ home, actions, moved }) => {
    const parent = path.join(home, "parent");
    const target = path.join(parent, "file");
    await fs.mkdir(parent);
    await fs.writeFile(target, "fixture");
    const plan = await actions.preview(target);
    await fs.rename(parent, `${parent}-old`);
    await fs.mkdir(parent);
    // Preserve the file inode while replacing its parent directory.
    await fs.rename(path.join(`${parent}-old`, "file"), target);
    await assert.rejects(actions.execute(plan.planId, true), /changed/);
    assert.equal(moved.length, 0);
  });
});

test("expired previews and overlapping execution cannot move files", async () => {
  await withEntryFixture(async ({ home }) => {
    let clock = 0;
    let finish;
    const moved = [];
    const shell = { trashItem: target => { moved.push(target); return new Promise(resolve => { finish = resolve; }); } };
    const actions = createEntryActions({ home, shell, now: () => clock, planTtlMs: 10 });
    const file = path.join(home, "file");
    await fs.writeFile(file, "fixture");
    const expired = await actions.preview(file);
    clock = 10;
    await assert.rejects(actions.execute(expired.planId, true), /expired/);
    const plan = await actions.preview(file);
    const execution = actions.execute(plan.planId, true);
    await assert.rejects(actions.execute(plan.planId, true), /in progress/);
    // Yield until the native shell boundary is reached, then finish that move.
    while (!finish) await new Promise(resolve => setImmediate(resolve));
    finish();
    await execution;
    await assert.rejects(actions.execute(plan.planId, true), /expired/);
    assert.deepEqual(moved, [file]);
  });
});

test("shell failures consume the plan and leave the move lock usable", async () => {
  await withEntryFixture(async ({ home }) => {
    const file = path.join(home, "file");
    await fs.writeFile(file, "fixture");
    let fail = true;
    const actions = createEntryActions({ home, shell: { trashItem: async () => { if (fail) throw new Error("Permission denied"); } } });
    const first = await actions.preview(file);
    await assert.rejects(actions.execute(first.planId, true), /permission-required:/);
    await assert.rejects(actions.execute(first.planId, true), /expired/);
    fail = false;
    const retry = await actions.preview(file);
    await actions.execute(retry.planId, true);
  });
});

test("filesystem metadata and native permission errors expose onboarding recovery without reusing a plan", async () => {
  const denied = Object.assign(new Error("Operation not permitted"), { code: "EPERM" });
  const blocked = createEntryActions({ shell: {}, fileSystem: { realpath: async () => { throw denied; } } });
  await assert.rejects(blocked.details("/valid/item"), /permission-required:/);
  await assert.rejects(blocked.preview("/valid/item"), /permission-required:/);
  await withEntryFixture(async ({ home }) => {
    const file = path.join(home, "item");
    await fs.writeFile(file, "fixture");
    const actions = createEntryActions({ shell: { trashItem: async () => { throw Object.assign(new Error("ACL blocked"), { code: "EACCES" }); } } });
    const preview = await actions.preview(file);
    await assert.rejects(actions.execute(preview.planId, true), /permission-required:/);
    await assert.rejects(actions.execute(preview.planId, true), /expired/);
    assert.equal(await fs.readFile(file, "utf8"), "fixture");
  });
});


test("only current-user Trash root gets a permanent-empty plan; snapshot excludes new arrivals and keeps links' targets", async () => {
  await withEntryFixture(async ({ base, home, shell, moved }) => {
    const trash = path.join(home, ".Trash");
    await fs.mkdir(path.join(trash, "folder"), { recursive: true });
    const retained = path.join(base, "retained");
    await fs.mkdir(retained);
    await fs.writeFile(path.join(retained, "keep"), "outside");
    await fs.writeFile(path.join(trash, "folder", "remove"), "owned");
    await fs.symlink(retained, path.join(trash, "folder", "nested-link"));
    await fs.symlink(retained, path.join(trash, "link"));
    const actions = createEntryActions({ home, shell });
    const preview = await actions.preview(trash);
    assert.equal(preview.action, "empty-trash");
    assert.equal(preview.itemCount, 2);
    await assert.rejects(actions.execute(preview.planId, true), /Action changed/);
    const confirmed = await actions.preview(trash);
    await fs.writeFile(path.join(trash, "new-arrival"), "retain");
    const result = await actions.execute(confirmed.planId, true, "empty-trash");
    assert.equal(result.removedCount, 2);
    assert.equal(result.remainingCount, 1);
    assert.deepEqual(result.failures, []);
    assert.deepEqual(await fs.readdir(trash), ["new-arrival"]);
    assert.equal(await fs.readFile(path.join(retained, "keep"), "utf8"), "outside");
    assert.deepEqual(moved, []);
    await assert.rejects(actions.execute(confirmed.planId, true, "empty-trash"), /expired/);
    const child = await actions.preview(path.join(trash, "new-arrival"));
    assert.equal(child.action, undefined);
    const alias = path.join(base, "home-alias");
    await fs.symlink(home, alias);
    assert.equal((await actions.preview(path.join(alias, ".Trash"))).action, "empty-trash");
  });
});

test("Trash rejects symbolic root, stale child and root replacement before any permanent removal", async () => {
  await withEntryFixture(async ({ base, home, shell }) => {
    const actions = createEntryActions({ home, shell });
    const trash = path.join(home, ".Trash");
    const retained = path.join(base, "outside");
    await fs.mkdir(retained);
    await fs.symlink(retained, trash);
    await assert.rejects(actions.preview(trash), /Trash path changed/);
    await fs.unlink(trash);
    await fs.mkdir(trash);
    await fs.writeFile(path.join(trash, "a"), "a");
    await fs.writeFile(path.join(trash, "b"), "b");
    const changed = await actions.preview(trash);
    await fs.rename(path.join(trash, "b"), path.join(base, "saved-b"));
    await fs.writeFile(path.join(trash, "b"), "replacement");
    await assert.rejects(actions.execute(changed.planId, true, "empty-trash"), /contents changed/);
    assert.equal(await fs.readFile(path.join(trash, "a"), "utf8"), "a");
    const replaced = await actions.preview(trash);
    await fs.rename(trash, path.join(base, "saved-trash"));
    await fs.mkdir(trash);
    await assert.rejects(actions.execute(replaced.planId, true, "empty-trash"), /Item changed/);
    assert.equal(await fs.readFile(path.join(base, "saved-trash", "a"), "utf8"), "a");
  });
});

test("partial permanent deletion reports failures and consumes the plan; a fresh preview retries", async () => {
  await withEntryFixture(async ({ home, shell }) => {
    const trash = path.join(home, ".Trash");
    await fs.mkdir(trash);
    await fs.writeFile(path.join(trash, "a"), "a");
    await fs.writeFile(path.join(trash, "b"), "b");
    let denied = true;
    const fileSystem = { ...fs, rm: async (target, options) => {
      if (denied && path.basename(target) === "b") throw Object.assign(new Error("ACL blocked"), { code: "EACCES" });
      return fs.rm(target, options);
    } };
    const actions = createEntryActions({ home, shell, fileSystem });
    const preview = await actions.preview(trash);
    const partial = await actions.execute(preview.planId, true, "empty-trash");
    assert.equal(partial.removedCount, 1);
    assert.equal(partial.remainingCount, 1);
    assert.match(partial.failures[0].message, /permission-required/);
    await assert.rejects(actions.execute(preview.planId, true, "empty-trash"), /expired/);
    denied = false;
    const retry = await actions.preview(trash);
    const done = await actions.execute(retry.planId, true, "empty-trash");
    assert.equal(done.remainingCount, 0);
    assert.equal((await fs.lstat(trash)).isDirectory(), true);
  });
});
