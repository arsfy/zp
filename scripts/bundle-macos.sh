#!/usr/bin/env bash
# Builds ZP.app and an update ZIP, plus a drag-to-install DMG with --dmg.
#
#   scripts/bundle-macos.sh [--target aarch64-apple-darwin] [--dmg]
#
# Signing and notarization are optional:
#   ZP_SIGN_IDENTITY   "Developer ID Application: …" (default: ad-hoc signature)
#   ZP_NOTARY_PROFILE  a `xcrun notarytool store-credentials` profile; when set, the app is
#                      notarized and stapled, along with the DMG when requested.
set -euo pipefail
cd "$(dirname "$0")/.."

target=""
build_dmg=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      if [[ $# -lt 2 || "$2" == --* || -z "$2" ]]; then
        echo "error: --target requires a Rust target triple" >&2
        exit 1
      fi
      target="$2"
      shift 2
      ;;
    --dmg)
      build_dmg=true
      shift
      ;;
    --help|-h)
      echo "Usage: $0 [--target aarch64-apple-darwin|x86_64-apple-darwin] [--dmg]"
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1 (see --help)" >&2
      exit 1
      ;;
  esac
done

identity="${ZP_SIGN_IDENTITY:--}"
if [[ -n "${ZP_NOTARY_PROFILE:-}" && "$identity" == "-" ]]; then
  echo "error: notarization requires ZP_SIGN_IDENTITY to be a Developer ID identity" >&2
  exit 1
fi
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=${target%%-*}
arch=${arch:-$(uname -m)}
[[ "$arch" == "arm64" ]] && arch=aarch64

echo "==> building zp $version ($arch)"
if [[ -n "$target" ]]; then
  cargo build --release -p zp-app --target "$target"
  binary="target/$target/release/zp"
else
  cargo build --release -p zp-app
  binary="target/release/zp"
fi

dist="dist"
app="$dist/ZP.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/zp"
python3 - "$version" "$app/Contents/Info.plist" <<'PY'
import os, plistlib, sys
with open("packaging/macos/Info.plist", "rb") as source:
    info = plistlib.load(source)
info["CFBundleShortVersionString"] = sys.argv[1].split("-")[0]
info["CFBundleVersion"] = os.environ.get("GITHUB_RUN_NUMBER", "1")
info["ZPReleaseVersion"] = sys.argv[1]
with open(sys.argv[2], "wb") as output:
    plistlib.dump(info, output)
PY

echo "==> icon"
iconutil -c icns packaging/macos/ZP.iconset -o "$app/Contents/Resources/ZP.icns"

echo "==> signing"
timestamp=(--timestamp=none)
if [[ "$identity" != "-" ]]; then
  timestamp=(--timestamp)
fi
codesign --force --options runtime "${timestamp[@]}" \
  --entitlements packaging/macos/entitlements.plist \
  --sign "$identity" "$app"
codesign --verify --strict --verbose=1 "$app"

zip="$dist/ZP-$version-darwin-$arch.zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
if [[ -n "${ZP_NOTARY_PROFILE:-}" ]]; then
  echo "==> notarizing application"
  xcrun notarytool submit "$zip" --keychain-profile "$ZP_NOTARY_PROFILE" --wait
  xcrun stapler staple "$app"
  rm -f "$zip"
  ditto -c -k --keepParent "$app" "$zip"
fi
echo "==> $zip"

if [[ "$build_dmg" == true ]]; then
  echo "==> creating DMG"
  staging=$(mktemp -d "${TMPDIR:-/tmp}/zp-dmg.XXXXXX")
  trap 'rm -rf "$staging"' EXIT
  ditto "$app" "$staging/ZP.app"
  ln -s /Applications "$staging/Applications"
  dmg="$dist/ZP-$version-darwin-$arch.dmg"
  hdiutil create -volname "ZP" -srcfolder "$staging" \
    -fs HFS+ -format UDZO -ov "$dmg"

  if [[ "$identity" != "-" ]]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
    codesign --verify --strict --verbose=1 "$dmg"
  fi
  if [[ -n "${ZP_NOTARY_PROFILE:-}" ]]; then
    echo "==> notarizing DMG"
    xcrun notarytool submit "$dmg" --keychain-profile "$ZP_NOTARY_PROFILE" --wait
    xcrun stapler staple "$dmg"
  fi
  echo "==> $dmg"
fi
