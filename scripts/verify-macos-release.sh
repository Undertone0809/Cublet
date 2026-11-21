#!/usr/bin/env bash
set -euo pipefail
[[ "$(uname -s)" == Darwin ]] || { echo 'macOS is required' >&2; exit 1; }
app_path="${1:?usage: verify-macos-release.sh APP [DMG] [arm64|x64]}"
dmg_path="${2:-}"
arch="${3:-arm64}"
case "$arch" in arm64) expected=arm64;; x64) expected=x86_64;; *) echo 'Unknown architecture' >&2; exit 1;; esac
verify_app() {
 local candidate="$1" executable bundle_id signature binary version minimum
 version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$candidate/Contents/Info.plist")"
 minimum="$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$candidate/Contents/Info.plist")"
 [[ "$minimum" == 13.0 ]] || { echo "Unexpected minimum macOS: $minimum" >&2; exit 1; }
 [[ -z "${EXPECTED_VERSION:-}" || "$version" == "$EXPECTED_VERSION" ]] || { echo "Unexpected version: $version" >&2; exit 1; }
 executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$candidate/Contents/Info.plist")"
 bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$candidate/Contents/Info.plist")"
 [[ "$bundle_id" == com.zeeland.codex-storage-manager.electron ]] || { echo 'Unexpected bundle identity' >&2; exit 1; }
 for binary in "$candidate/Contents/MacOS/$executable" "$candidate/Contents/Resources/bin/codex-core" "$candidate/Contents/Resources/bin/cublet-cleaner"; do
  [[ -x "$binary" && "$(lipo -archs "$binary")" == "$expected" ]] || { echo "Wrong architecture: $binary" >&2; exit 1; }
  if [[ "${REQUIRE_SIGNED:-0}" == 1 ]]; then
   codesign --verify --strict "$binary"
   signature="$(codesign -dv --verbose=4 "$binary" 2>&1)"
   [[ "$signature" == *"TeamIdentifier=QRP7F2GTGJ"* && "$signature" == *"Authority=Developer ID Application: Foundria, LLC"* ]] || { echo 'Unexpected signing identity' >&2; exit 1; }
  fi
 done
 if [[ "$(uname -m)" == "$expected" ]]; then
  for binary in "$candidate/Contents/Resources/bin/codex-core" "$candidate/Contents/Resources/bin/cublet-cleaner"; do
   [[ "$("$binary" --mcp-version)" == "$version" ]] || { echo "MCP version does not match Cublet: $binary" >&2; exit 1; }
  done
 fi
 if [[ "${REQUIRE_SIGNED:-0}" == 1 ]]; then
  codesign --verify --deep --strict "$candidate"
  spctl --assess --type execute --verbose=2 "$candidate"
 fi
 if [[ "${REQUIRE_NOTARIZATION:-0}" == 1 ]]; then xcrun stapler validate "$candidate"; fi
}
verify_app "$app_path"
if [[ -n "$dmg_path" ]]; then
 hdiutil verify "$dmg_path"
 if [[ "${REQUIRE_SIGNED:-0}" == 1 ]]; then codesign --verify --strict "$dmg_path"; fi
 if [[ "${REQUIRE_NOTARIZATION:-0}" == 1 ]]; then
  xcrun stapler validate "$dmg_path"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg_path"
 fi
 mount_point="$(mktemp -d "${TMPDIR:-/tmp}/cublet-dmg.XXXXXX")"
 trap 'hdiutil detach "$mount_point" >/dev/null 2>&1 || true; rmdir "$mount_point" 2>/dev/null || true' EXIT
 hdiutil attach -nobrowse -readonly -mountpoint "$mount_point" "$dmg_path" >/dev/null
 verify_app "$mount_point/Cublet.app"
fi
echo "Cublet $arch bundle verification passed"
