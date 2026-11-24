# ZP

<a href="https://github.com/arsfy/zp/releases/latest">
    <img alt="Release" src="https://img.shields.io/github/v/release/arsfy/zp?style=flat-square">
</a>
<a href="https://github.com/arsfy/zp/actions/workflows/ci.yml">
    <img alt="CI" src="https://img.shields.io/github/actions/workflow/status/arsfy/zp/ci.yml?branch=main&amp;style=flat-square&amp;label=CI">
</a>
<a href="#">
    <img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square">
</a>

[English](README.md) | 简体中文 | [繁體中文](README.zh-HK.md)

> 注意：本项目仍处于早期开发阶段，可能存在各类问题，未来也可能引入破坏性变更（breaking changes），且目前尚未提供多语言国际化（i18n）支持。

[zp](https://github.com/arsfy/zp) 是一款轻量、原生渲染、高性能的 GUI Agent 客户端（Harness）。它采用了类似 Pi 的极简 Agent 设计哲学，并结合了源自 VS Code / Zed 的面向项目的设计方式与 Git 管理体验。

*安装包体积约 18–24 MB，安装后的可执行文件仅 30–45 MB。*

<img width="100%" alt="Home" src="https://github.com/user-attachments/assets/81ac4837-feef-4730-b309-c15fcebbcd08" />

*超小体积、极低开销、零追踪器、拒绝臃肿——这就是 ZP 的设计哲学。*

目前已支持：

- **Agent 与工具**：极简内置工具集（`read`、`write`、`edit`、`bash`、`grep`、`find`、`ls`、`todo`）、交互式提问 `ask`、并行只读子代理 `subagent` 以及 `/plan` 规划模式。
- **权限与自动审查**：四级权限模式，结合静态 Shell 语法分类、会话私有临时目录，以及基于 LLM 或 TypeSafe Jev 的命令自动安全审查。
- **会话与检查点**：可分支的树状会话、运行中消息队列（支持 `Edit` / `Withdraw` / `Interrupt & Send`）、自动上下文压缩，以及按轮次的文件快照与回滚。
- **项目 GUI、Git 与终端**：基于 GPUI 的原生界面，支持 `@file` 补全、流式预览与语法高亮 Diff、内置终端模拟器，以及支持 AI 生成提交信息的 Git 面板。
- **扩展性与模型提供商**：兼容 [Agent Skills](https://agentskills.io/specification)（`$skill`）、`/template` 提示词模板、MCP（stdio 与 HTTP）、多厂商 API / OAuth 登录，以及命令行前端 `zp-cli`。

## 安装

从 [Releases](https://github.com/arsfy/zp/releases) 页面下载 ZP。

- **Stable（稳定版）**：[最新发布版本](https://github.com/arsfy/zp/releases/latest)，标签格式为 `vX.Y.Z`。
- **Preview（预览版）**：Releases 页面中标记为 **Pre-release** 的版本，标签格式为 `vX.Y.Z-rc.N`，会优先提供新功能，但稳定性可能相对较低。

### macOS

打开 `.dmg` 并将 **ZP** 拖入 **Applications（应用程序）**。

未经 Apple 公证（notarization）构建的发行版仅包含临时签名（ad-hoc signed），macOS 可能会拒绝打开（提示“ZP 已损坏”或“Apple 无法验证 ZP”）。只需移除一次下载隔离属性，即可正常打开 ZP：

```sh
xattr -dr com.apple.quarantine /Applications/ZP.app
```

### Windows

运行 `-setup.exe` 安装程序，或将便携版 `.zip` 解压到任意位置后运行 `zp.exe`。

### Linux

在 ARM64 架构上使用 `aarch64` 安装包，在 Intel 和 AMD 系统上使用 `x86_64` 安装包。
对应的 Debian 软件包在文件名中使用 `arm64` 和 `amd64`。

```sh
# AppImage
chmod +x ZP-<version>-linux-x86_64.AppImage
./ZP-<version>-linux-x86_64.AppImage

# Debian / Ubuntu
sudo apt install ./zp_<version>_amd64.deb

# 压缩包
tar -xzf ZP-<version>-linux-x86_64.tar.gz
./zp-<version>/zp
```

### 使用方式

运行 `zp` 可恢复上次会话中的项目，或运行 `zp <DIR>` 将指定文件夹作为项目打开。设置、会话及其他数据保存在 `~/.zp` 下（可通过设置环境变量 `ZP_HOME` 指定其他目录）。

## 更新

ZP 启动后会检查 GitHub 上的更新，之后每 6 小时检查一次。你可以在 **Settings › About** 中关闭自动检查。发现更新时，标题栏会出现 **Update** 按钮。在确认之前不会下载任何内容，且每次下载在安装前都会校验签名发布清单（signed release manifest）与 SHA-256 哈希。

- **更新通道**：在 **Settings › About** 中选择 **Stable** 或 **Preview**。Preview 通道还会推送预览版和 RC 版本。切回 Stable 绝不会降级 ZP：你将保持当前版本，直到有更新的稳定版发布。
- **macOS、Windows 便携版、AppImage 与 Linux 压缩包**：ZP 会替换自身并重启。已打开的项目、标签页和未发送的草稿都会自动恢复。如果新版本启动失败，会自动回退至旧版本。
- **Windows 安装程序版**：会自动为你启动下载好的安装程序。
- **Debian 软件包**：会在系统的软件包安装器中打开 `.deb`，以便继续由包管理器统一管理。

在 macOS 上，未经 Apple 公证的发行版采用临时签名，因此 macOS 会将每个新版本视为全新应用：更新后，ZP 从钥匙串读取 API 密钥前会再次询问授权（请选择 **始终允许 / Always Allow**），并可能再次请求文件夹访问权限。

从源码构建的版本可以检查更新，但无法直接安装更新，因为其中未嵌入发布验证公钥。请手动下载发行包或重新从源码构建。

## 从源码构建

### 环境要求

- [Rust](https://rustup.rs) 1.95 或更高版本，以及 Git。
- **macOS**：Xcode Command Line Tools（`xcode-select --install`）。
- **Windows**：安装了 *使用 C++ 的桌面开发* 工作负载的 Visual Studio Build Tools。
- **Linux**（Debian / Ubuntu）：

  ```sh
  sudo apt-get install -y build-essential pkg-config \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-dev libx11-xcb-dev \
    libxcb1-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb-randr0-dev libxcb-xinput-dev \
    libegl1-mesa-dev libgles2-mesa-dev libvulkan-dev libglib2.0-dev libfontconfig-dev \
    libdbus-1-dev libsecret-1-dev
  ```

### 构建与运行

```sh
git clone https://github.com/arsfy/zp.git
cd zp
cargo run --release -p zp-app              # 恢复上次会话
cargo run --release -p zp-app -- ~/my-app  # 打开指定项目
```

编译后的可执行文件位于 `target/release/zp`（Windows 下为 `zp.exe`）。

### 打包

打包脚本会构建 Release 二进制文件并将安装包输出到 `dist/` 目录：

```sh
# macOS：生成 ZP.app 及其 .zip；添加 --dmg 可生成磁盘映像。
# 除非设置了 ZP_SIGN_IDENTITY，否则使用临时签名（ad-hoc signed）。
scripts/bundle-macos.sh --dmg
scripts/bundle-macos.sh --target x86_64-apple-darwin --dmg

# Linux：生成 .tar.gz；当安装了 appimagetool 时额外生成 AppImage，
# 当 dpkg-deb 可用时额外生成 .deb。
scripts/package-linux.sh
```

```powershell
# Windows：生成便携版 .zip；当安装了 Inno Setup (iscc) 时额外生成安装程序。
# 使用 ImageMagick (magick) 生成图标。
pwsh scripts/package-windows.ps1
```

### 开发

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

工作区中还包含 `zp-cli`，这是基于同一 Agent 核心的终端前端（`cargo run -p zp-cli -- --help`）。

## 许可证

ZP 采用 [Apache License, Version 2.0](LICENSE-APACHE) 或 [MIT License](LICENSE-MIT) 双许可证授权，你可任选其一。
