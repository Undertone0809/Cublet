// Cublet-owned adapter for electron-updater's deferred config and staging-ID reads.
// This intentionally implements only the factory/value contract used by pinned
// electron-updater@6.8.11: no setter, `has`, or other lazy-val API is supported.
// it does not contain or derive from lazy-val's implementation.
class Lazy {
  #factory;
  #cached;
  #started = false;

  constructor(factory) {
    this.#factory = factory;
  }

  get value() {
    if (!this.#started) {
      // Cache the returned promise itself so concurrent reads and rejected
      // asynchronous reads share one operation. A synchronous throw can retry.
      this.#cached = this.#factory();
      this.#started = true;
      this.#factory = undefined;
    }
    return this.#cached;
  }
}

module.exports = { Lazy };
