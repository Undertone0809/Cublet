# Cublet

[简体中文](README.zh-CN.md)

Cublet is a local disk space analyzer for macOS. Use Current usage to see which directories take up space, Growth to compare two saved scans, and Applications to review app copies and related files before removal.

[Download for macOS](https://cublet.cc/#download) · [Installation and help](https://cublet.cc/help/) · [Release notes](https://cublet.cc/changelog/)

![Cublet Current usage in a native development build with sample data](https://cublet.cc/images/cublet-space.webp)

Cublet supports macOS 13 and later on Apple Silicon and Intel. Disk scans and their history stay on your Mac. Official builds contact the release server to check for and download updates. macOS permissions determine which locations a scan can read. Growth needs two comparable scans, and moving items to Trash reclaims space when the Trash is emptied.

## Build on macOS

Install Git, the Xcode Command Line Tools, Node.js 22 or later, and stable Rust with Cargo. Clone the repository so the packaging tools can record the exact source commit.

```sh
git clone https://github.com/Undertone0809/Cublet.git cublet
cd cublet
npm ci --include=dev
npm run build
cargo test --locked --manifest-path rust-core/Cargo.toml -- --test-threads=1

profile=$(mktemp -d "${TMPDIR:-/tmp}/cublet-dev.XXXXXX")
MSM_DISK_CACHE_ROOT="$profile/cache" CODEX_HOME="$profile/codex" \
  npm run electron:dev -- --user-data-dir="$profile/profile"
```

For a local app bundle, install `rustup component add rust-docs` for the active toolchain, then run `CSC_IDENTITY_AUTO_DISCOVERY=false npm run electron:package`. The Rust documentation component provides the standard library license files included in the bundle. Local packages have the official updater disabled. To cross-build, install the matching Rust target and set `CUBLET_ARCH=arm64` or `CUBLET_ARCH=x64`. Use the isolated development command above for testing alongside an installed copy.

## Contribute

Read [CONTRIBUTING.md](CONTRIBUTING.md) for tests and issue guidelines. Report vulnerabilities using [the private reporting instructions](SECURITY.md). Cublet is an independent project.

Cublet's own code uses the [MIT License](LICENSE). The source and distributions retain [third-party notices](THIRD_PARTY_NOTICES.md).
