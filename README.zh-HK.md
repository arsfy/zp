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

[English](README.md) | [简体中文](README.zh-CN.md) | 繁體中文

> 注意：本專案仍處於早期開發階段，可能存在各類問題，未來亦可能引入破壞性變更（breaking changes），且目前尚未提供多語言國際化（i18n）支援。

[zp](https://github.com/arsfy/zp) 是一款輕量、原生渲染、高效能的 GUI Agent 客戶端（Harness）。它採用了類似 Pi 的極簡 Agent 設計哲學，並結合了源自 VS Code / Zed 的專案導向設計方式與 Git 管理體驗。

*安裝包體積約 18–24 MB，安裝後的可執行檔僅 30–45 MB。*

<img width="100%" alt="Home" src="https://github.com/user-attachments/assets/81ac4837-feef-4730-b309-c15fcebbcd08" />

*超小體積、極低開銷、零追蹤器、拒絕臃腫——這就是 ZP 的設計哲學。*

目前已支援：

- **Agent 與工具**：極簡內置工具集（`read`、`write`、`edit`、`bash`、`grep`、`find`、`ls`、`todo`）、互動式提問 `ask`、並行唯讀子代理 `subagent` 以及 `/plan` 規劃模式。
- **權限與自動審查**：四級權限模式，結合靜態 Shell 語法分類、工作階段私有暫存目錄，以及基於 LLM 或 TypeSafe Jev 的指令自動安全審查。
- **工作階段與檢查點**：可分支的樹狀工作階段、執行中訊息佇列（支援 `Edit` / `Withdraw` / `Interrupt & Send`）、自動上下文壓縮，以及按輪次的檔案快照與回滾。
- **專案 GUI、Git 與終端機**：基於 GPUI 的原生介面，支援 `@file` 補全、串流預覽與語法高亮 Diff、內置終端機模擬器，以及支援 AI 生成提交訊息的 Git 面板。
- **擴展性與模型供應商**：相容 [Agent Skills](https://agentskills.io/specification)（`$skill`）、`/template` 提示詞範本、MCP（stdio 與 HTTP）、多廠商 API / OAuth 登入，以及命令列前端 `zp-cli`。

## 安裝

從 [Releases](https://github.com/arsfy/zp/releases) 頁面下載 ZP。

- **Stable（穩定版）**：[最新發佈版本](https://github.com/arsfy/zp/releases/latest)，標籤格式為 `vX.Y.Z`。
- **Preview（預覽版）**：Releases 頁面中標記為 **Pre-release** 的版本，標籤格式為 `vX.Y.Z-rc.N`，會優先提供新功能，但穩定性可能相對較低。

### macOS

打開 `.dmg` 並將 **ZP** 拖入 **Applications（應用程式）**。

未經 Apple 公證（notarization）建置的發佈版僅包含臨時簽署（ad-hoc signed），macOS 可能會拒絕打開（提示「ZP 已損毀」或「Apple 無法驗證 ZP」）。只需移除一次下載隔離屬性，即可正常打開 ZP：

```sh
xattr -dr com.apple.quarantine /Applications/ZP.app
```

### Windows

執行 `-setup.exe` 安裝程式，或將便攜版 `.zip` 解壓到任意位置後執行 `zp.exe`。

### Linux

在 ARM64 架構上使用 `aarch64` 安裝包，在 Intel 和 AMD 系統上使用 `x86_64` 安裝包。
對應的 Debian 套件在檔案名稱中使用 `arm64` 和 `amd64`。

```sh
# AppImage
chmod +x ZP-<version>-linux-x86_64.AppImage
./ZP-<version>-linux-x86_64.AppImage

# Debian / Ubuntu
sudo apt install ./zp_<version>_amd64.deb

# 壓縮包
tar -xzf ZP-<version>-linux-x86_64.tar.gz
./zp-<version>/zp
```

### 使用方式

執行 `zp` 可恢復上次工作階段中的專案，或執行 `zp <DIR>` 將指定資料夾作為專案打開。設定、工作階段及其他資料儲存在 `~/.zp` 下（可透過設定環境變數 `ZP_HOME` 指定其他目錄）。

## 更新

ZP 啟動後會檢查 GitHub 上的更新，之後每 6 小時檢查一次。你可以在 **Settings › About** 中關閉自動檢查。發現更新時，標題列會出現 **Update** 按鈕。在確認之前不會下載任何內容，且每次下載在安裝前都會校驗簽署發佈清單（signed release manifest）與 SHA-256 雜湊值。

- **更新通道**：在 **Settings › About** 中選擇 **Stable** 或 **Preview**。Preview 通道還會推送預覽版和 RC 版本。切回 Stable 絕不會降級 ZP：你將保持目前版本，直到有更新的穩定版發佈。
- **macOS、Windows 便攜版、AppImage 與 Linux 壓縮包**：ZP 會替換自身並重啟。已打開的專案、分頁和未傳送的草稿都會自動恢復。如果新版本啟動失敗，會自動回退至舊版本。
- **Windows 安裝程式版**：會自動為你啟動下載好的安裝程式。
- **Debian 套件**：會在系統的套件安裝器中打開 `.deb`，以便繼續由套件管理器統一管理。

在 macOS 上，未經 Apple 公證的發佈版採用臨時簽署，因此 macOS 會將每個新版本視為全新應用程式：更新後，ZP 從鑰匙圈讀取 API 金鑰前會再次詢問授權（請選擇 **總是允許 / Always Allow**），並可能再次請求資料夾存取權限。

從原始碼建置的版本可以檢查更新，但無法直接安裝更新，因為其中未嵌入發佈驗證公鑰。請手動下載發佈包或重新從原始碼建置。

## 從原始碼建置

### 環境需求

- [Rust](https://rustup.rs) 1.95 或更高版本，以及 Git。
- **macOS**：Xcode Command Line Tools（`xcode-select --install`）。
- **Windows**：安裝了 *使用 C++ 的桌面開發* 工作負載的 Visual Studio Build Tools。
- **Linux**（Debian / Ubuntu）：

  ```sh
  sudo apt-get install -y build-essential pkg-config \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-dev libx11-xcb-dev \
    libxcb1-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb-randr0-dev libxcb-xinput-dev \
    libegl1-mesa-dev libgles2-mesa-dev libvulkan-dev libglib2.0-dev libfontconfig-dev \
    libdbus-1-dev libsecret-1-dev
  ```

### 建置與執行

```sh
git clone https://github.com/arsfy/zp.git
cd zp
cargo run --release -p zp-app              # 恢復上次工作階段
cargo run --release -p zp-app -- ~/my-app  # 打開指定專案
```

編譯後的可執行檔位於 `target/release/zp`（Windows 下為 `zp.exe`）。

### 打包

打包腳本會建置 Release 二進位檔案並將安裝包輸出到 `dist/` 目錄：

```sh
# macOS：生成 ZP.app 及其 .zip；加上 --dmg 可生成磁碟映像。
# 除非設定了 ZP_SIGN_IDENTITY，否則使用臨時簽署（ad-hoc signed）。
scripts/bundle-macos.sh --dmg
scripts/bundle-macos.sh --target x86_64-apple-darwin --dmg

# Linux：生成 .tar.gz；當安裝了 appimagetool 時額外生成 AppImage，
# 當 dpkg-deb 可用時額外生成 .deb。
scripts/package-linux.sh
```

```powershell
# Windows：生成便攜版 .zip；當安裝了 Inno Setup (iscc) 時額外生成安裝程式。
# 使用 ImageMagick (magick) 生成圖示。
pwsh scripts/package-windows.ps1
```

### 開發

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

工作區中還包含 `zp-cli`，這是基於同一 Agent 核心的終端機前端（`cargo run -p zp-cli -- --help`）。

## 授權條款

ZP 採用 [Apache License, Version 2.0](LICENSE-APACHE) 或 [MIT License](LICENSE-MIT) 雙授權條款，你可任選其一。
