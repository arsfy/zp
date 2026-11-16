#!/usr/bin/env bash
# Writes dist/latest.json, the update feed, from the built artifacts in dist/.
#
#   ZP_RELEASE_URL  base URL of the release downloads
#                   (default: https://github.com/arsfy/zp/releases/download/v<version>)
#   ZP_RELEASE_NOTES  text shown in the update dialog
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
base=${ZP_RELEASE_URL:-https://github.com/arsfy/zp/releases/download/v$version}
notes=${ZP_RELEASE_NOTES:-}

sha() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

entries=()
add() { # platform file
  local file="dist/$2"
  [[ -f "$file" ]] || return 0
  entries+=("    \"$1\": {\"url\": \"$base/$2\", \"sha256\": \"$(sha "$file")\"}")
}
for arch in aarch64 x86_64; do
  add "darwin-$arch" "ZP-$version-darwin-$arch.zip"
  add "linux-$arch" "ZP-$version-linux-$arch.AppImage"
done
add "windows-x86_64" "ZP-$version-windows-x86_64-setup.exe"

{
  echo "{"
  echo "  \"version\": \"$version\","
  printf '  "notes": %s,\n' "$(printf '%s' "$notes" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')"
  echo "  \"platforms\": {"
  (IFS=$'\n'; echo "${entries[*]:-}" | sed '$!s/$/,/')
  echo "  }"
  echo "}"
} > dist/latest.json
cat dist/latest.json
