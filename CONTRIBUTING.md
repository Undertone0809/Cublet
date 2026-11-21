# Contributing to Cublet

For bugs, describe the outcome you wanted, what happened, your Cublet and macOS versions, and the steps to reproduce. Use a synthetic directory or disposable test app. Remove personal paths, filenames, and scan data from logs and screenshots. Use the [security reporting process](SECURITY.md) for suspected vulnerabilities.

Follow the [local setup](README.md#build-on-macos) to reproduce issues. Keep pull requests focused and describe which workflow you verified.

```sh
npm run build
node --test electron/*.test.cjs src/lib/*.test.mjs scripts/*.test.mjs
cargo fmt --manifest-path rust-core/Cargo.toml -- --check
cargo test --locked --manifest-path rust-core/Cargo.toml -- --test-threads=1
```

Run the relevant native fixture from `scripts/` when changing scan behavior, permissions, or filesystem actions. Build the debug Rust sidecar with `npm run electron:build:core` first. Use owned fixtures for operations that move or delete files.
