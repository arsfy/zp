#!/usr/bin/env bash
# Builds Linux packages of ZP into dist/:
#   ZP-<version>-linux-<arch>.tar.gz   always
#   ZP-<version>-linux-<arch>.AppImage when `appimagetool` is on PATH
#   zp_<version>_<debarch>.deb         when `dpkg-deb` is on PATH
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(uname -m)
cargo build --release -p zp-app
binary=target/release/zp
mkdir -p dist

# Plain archive.
stage=$(mktemp -d)/zp-$version
mkdir -p "$stage"
cp "$binary" "$stage/zp"
cp packaging/linux/zp.desktop assets/icon.png LICENSE-MIT LICENSE-APACHE "$stage/"
tar -C "$(dirname "$stage")" -czf "dist/ZP-$version-linux-$arch.tar.gz" "zp-$version"

# AppImage.
if command -v appimagetool >/dev/null; then
  appdir=$(mktemp -d)/ZP.AppDir
  mkdir -p "$appdir/usr/bin"
  cp "$binary" "$appdir/usr/bin/zp"
  cp packaging/linux/zp.desktop "$appdir/zp.desktop"
  cp assets/icon.png "$appdir/zp.png"
  cat > "$appdir/AppRun" <<'RUN'
#!/bin/sh
exec "$(dirname "$(readlink -f "$0")")/usr/bin/zp" "$@"
RUN
  chmod +x "$appdir/AppRun"
  ARCH=$arch appimagetool "$appdir" "dist/ZP-$version-linux-$arch.AppImage"
fi

# Debian package.
if command -v dpkg-deb >/dev/null; then
  case "$arch" in x86_64) debarch=amd64 ;; aarch64) debarch=arm64 ;; *) debarch=$arch ;; esac
  root=$(mktemp -d)/zp_$version
  deb_version=${version/-/\~}
  mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/applications" "$root/usr/share/icons/hicolor/1024x1024/apps"
  cp "$binary" "$root/usr/bin/zp"
  cp packaging/linux/zp.desktop "$root/usr/share/applications/zp.desktop"
  cp assets/icon.png "$root/usr/share/icons/hicolor/1024x1024/apps/zp.png"
  cat > "$root/DEBIAN/control" <<CONTROL
Package: zp
Version: $deb_version
Section: devel
Priority: optional
Architecture: $debarch
Depends: libxkbcommon0, libxkbcommon-x11-0, libvulkan1, libwayland-client0, libsecret-1-0
Maintainer: ZP <zp@users.noreply.github.com>
Description: A lightweight, native and high performance GUI Harness
CONTROL
  # The file name keeps the release version: GitHub renames `~` in uploaded assets.
  dpkg-deb --build --root-owner-group "$root" "dist/zp_${version}_${debarch}.deb"
fi
ls -1 dist
