# ZP

<a href="https://github.com/arsfy/zp/releases/latest">
    <img alt="Release" src="https://img.shields.io/github/v/release/arsfy/zp?style=flat-square">
</a>
<a href="https://github.com/arsfy/zp/actions/workflows/release.yml">
    <img alt="Build" src="https://img.shields.io/github/actions/workflow/status/arsfy/zp/release.yml?branch=main&style=flat-square">
</a>
<a href="https://github.com/arsfy/zp/actions/workflows/ci.yml">
    <img alt="CI" src="https://img.shields.io/github/actions/workflow/status/arsfy/zp/ci.yml?branch=main&amp;style=flat-square&amp;label=CI">
</a>
<img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square">

> Note: This is an early-stage project. It may contain various issues and could introduce breaking changes in the future.

[zp](https://github.com/arsfy/zp) is a lightweight, natively rendered, high-performance GUI harness. It adopts a minimalist Agent design philosophy similar to Pi, combined with a project-oriented design approach and Git management inspired by VS Code / Zed.

<img width="100%" alt="Home" src="https://github.com/user-attachments/assets/0fd8d3b2-3e30-4e5c-9056-8f678ce9f2b9" />

## Installation

Download ZP from the [Releases](https://github.com/arsfy/zp/releases) page.

- **Stable**: the [latest release](https://github.com/arsfy/zp/releases/latest). Stable versions are tagged `vX.Y.Z`.
- **Preview**: releases marked **Pre-release** on the Releases page, tagged `vX.Y.Z-preview.N` or `vX.Y.Z-rc.N`. They get new features first and may be less stable.

### macOS

Open the `.dmg` and drag **ZP** to **Applications**. The application is signed and notarized.

### Windows

Run the `-setup.exe` installer, or extract the portable `.zip` anywhere and run `zp.exe`.

### Linux

```sh
# AppImage
chmod +x ZP-<version>-linux-x86_64.AppImage
./ZP-<version>-linux-x86_64.AppImage

# Debian / Ubuntu
sudo apt install ./zp_<version>_amd64.deb

# Archive
tar -xzf ZP-<version>-linux-x86_64.tar.gz
./zp-<version>/zp
```

### Usage

Run `zp` to restore the projects from your last session, or `zp <DIR>` to open a folder as a project. Settings, sessions and other data are stored in `~/.zp` (set `ZP_HOME` to use another directory).

## Updates

ZP checks GitHub for updates after it starts and then every six hours. You can turn this off in **Settings › About**. When an update is found, an **Update** button appears in the title bar. Nothing is downloaded until you confirm, and every download is checked against a signed release manifest and its SHA-256 hash before it is installed.

- **Update channel**: choose **Stable** or **Preview** in **Settings › About**. Preview also offers preview and RC releases. Switching back to Stable never downgrades ZP: you stay on your current version until a newer stable release is available.
- **macOS, Windows portable, AppImage and Linux archive**: ZP replaces itself and restarts. Your open projects, tabs and unsent drafts are restored. If the new version fails to start, the previous version is restored automatically.
- **Windows installer**: the downloaded installer is started for you.
- **Debian package**: the `.deb` is opened in your system's package installer, so it stays managed by the package manager.

Builds made from source can check for updates but cannot install them, because they don't embed the release verification key. Download releases manually, or rebuild from source.

## Building from source

### Requirements

- [Rust](https://rustup.rs) 1.95 or newer and Git.
- **macOS**: Xcode Command Line Tools (`xcode-select --install`).
- **Windows**: Visual Studio Build Tools with the *Desktop development with C++* workload.
- **Linux** (Debian / Ubuntu):

  ```sh
  sudo apt-get install -y build-essential pkg-config \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-dev libx11-xcb-dev \
    libxcb1-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb-randr0-dev libxcb-xinput-dev \
    libegl1-mesa-dev libgles2-mesa-dev libvulkan-dev libglib2.0-dev libfontconfig-dev \
    libdbus-1-dev libsecret-1-dev
  ```

### Build and run

```sh
git clone https://github.com/arsfy/zp.git
cd zp
cargo run --release -p zp-app              # restore the last session
cargo run --release -p zp-app -- ~/my-app  # open a project
```

The executable is written to `target/release/zp` (`zp.exe` on Windows).

### Packages

The packaging scripts build release binaries and write packages to `dist/`:

```sh
# macOS: ZP.app and its .zip; add --dmg for a disk image.
# Ad-hoc signed unless ZP_SIGN_IDENTITY is set.
scripts/bundle-macos.sh --dmg
scripts/bundle-macos.sh --target x86_64-apple-darwin --dmg

# Linux: .tar.gz, plus an AppImage when appimagetool is installed
# and a .deb when dpkg-deb is available.
scripts/package-linux.sh
```

```powershell
# Windows: portable .zip, plus an installer when Inno Setup (iscc) is installed.
# ImageMagick (magick) generates the icon.
pwsh scripts/package-windows.ps1
```

### Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

The workspace also includes `zp-cli`, a terminal front end for the same agent (`cargo run -p zp-cli -- --help`).

## License

ZP is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT), at your option.
