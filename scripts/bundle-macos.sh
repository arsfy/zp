#!/usr/bin/env bash
# Builds ZP.app and a zip for releases.
#
#   scripts/bundle-macos.sh [--target aarch64-apple-darwin]
#
# Signing and notarization are optional:
#   ZP_SIGN_IDENTITY   "Developer ID Application: …" (default: ad-hoc signature)
#   ZP_NOTARY_PROFILE  a `xcrun notarytool store-credentials` profile; when set, the app is
#                      notarized and stapled.
set -euo pipefail
cd "$(dirname "$0")/.."

target=""
if [[ "${1:-}" == "--target" ]]; then
  target="$2"
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
sed "s/__VERSION__/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"

echo "==> icon"
iconset=$(mktemp -d)/ZP.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size assets/icon.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double assets/icon.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/ZP.icns"

echo "==> signing"
identity="${ZP_SIGN_IDENTITY:--}"
codesign --force --options runtime --timestamp=none \
  --entitlements packaging/macos/entitlements.plist \
  --sign "$identity" "$app"
codesign --verify --strict --verbose=1 "$app"

zip="$dist/ZP-$version-darwin-$arch.zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"

if [[ -n "${ZP_NOTARY_PROFILE:-}" ]]; then
  echo "==> notarizing"
  xcrun notarytool submit "$zip" --keychain-profile "$ZP_NOTARY_PROFILE" --wait
  xcrun stapler staple "$app"
  rm -f "$zip"
  ditto -c -k --keepParent "$app" "$zip"
fi

echo "==> $zip"
