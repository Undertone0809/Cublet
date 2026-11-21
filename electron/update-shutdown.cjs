// Both normal quit and update installation use the same bounded scan checkpoint.
async function checkpointScans(scanIds, cancel, timeoutMs = 2000) {
  if (!scanIds.length) return;
  let timer;
  try {
    await Promise.race([
      Promise.allSettled(scanIds.map(scanId => cancel(scanId))),
      new Promise(resolve => { timer = setTimeout(resolve, timeoutMs); }),
    ]);
  } finally { clearTimeout(timer); }
}
async function prepareUpdateShutdown({ waitForUninstall, checkpoint, isCurrent = () => true }) {
  // An uninstall failure still means the operation has settled. Never truncate it.
  await Promise.resolve().then(waitForUninstall).catch(() => {});
  if (isCurrent()) await checkpoint();
}
function createMutationTracker() {
  const pending = new Set();
  return {
    track(value) {
      const promise = Promise.resolve(value);
      pending.add(promise);
      promise.then(() => pending.delete(promise), () => pending.delete(promise));
      return promise;
    },
    async settle() {
      // The caller closes the mutation admission gate before draining. Recheck
      // the set so already accepted work added within the same turn is included.
      while (pending.size) await Promise.allSettled([...pending]);
    },
    get size() { return pending.size; },
  };
}
module.exports = { checkpointScans, prepareUpdateShutdown, createMutationTracker };
