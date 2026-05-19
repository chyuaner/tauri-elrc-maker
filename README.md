# LRC Maker Enhanced

基於 Tauri v2 + Next.js 的逐字歌詞製作工具，支援 Windows / Linux 桌面。

---

## 系統需求

### 所有平台

| 需求 | 版本 | 用途 |
|------|------|------|
| Node.js | 18+ | 前端建置 |
| Rust | stable (1.75+) | Tauri 後端 |
| npm | 9+ | 套件管理 |

### Linux 額外需求

> ⚠️ **Linux 平台有多項 Workaround，缺少以下套件會導致功能異常**

#### 必要套件

```bash
# WebKitGTK（Tauri on Linux 的 WebView 核心）
sudo apt install libwebkit2gtk-4.1-dev

# GTK3 開發函式庫（自訂 HeaderBar 標題列）
sudo apt install libgtk-3-dev

# GStreamer 媒體解碼（音訊播放核心）
sudo apt install \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav

# AppImage 執行工具（打包用）
sudo apt install fuse
```

#### 播放 M4A / AAC 格式需要 ffmpeg

```bash
sudo apt install ffmpeg
```

> **原因**：GStreamer 在多數 Linux 發行版預設缺少 M4A/AAC 解碼器（需 `gstreamer1.0-plugins-bad` 或 `gstreamer1.0-libav`）。本應用改由 ffmpeg 在接收到 M4A/AAC 時自動轉成 WAV 格式播放。若未安裝 ffmpeg，M4A/AAC 檔案**無法播放**。

---

## Linux 已知限制與 Workaround 說明

本應用在 Linux 上有數個針對 GStreamer + WebKitGTK 相容性問題的 Workaround，以下說明各問題的根因與解法，方便在不同機器排查問題。

### 1. GStreamer 無法播放 Blob URL

**問題**：WebKitGTK 底層依賴 GStreamer 解碼，但 GStreamer 無法播放前端以 `URL.createObjectURL(file)` 建立的 `blob://` URL。

**Workaround**：
- 在 WebView 初始化時注入 JS，monkey-patch `URL.createObjectURL`
- 攔截到媒體 Blob 時，同步 POST 到本機 HTTP Server（`port 12435`）儲存為 `/tmp/` 暫存檔
- 前端改用 `http://127.0.0.1:12435/media/<檔名>` 播放
- GStreamer 可正常串流標準 HTTP URL

**副作用**：暫存檔存於 `/tmp/`，不會自動清除（重開機才消失）。

### 2. GStreamer 串流模式下 `duration` 回傳 `Infinity`

**問題**：GStreamer 以 HTTP 串流播放時，無法預知總長度，導致 `<audio>.duration` 回傳 `Infinity`。

**Workaround（FLAC）**：在後端接收 FLAC 檔案時，直接解析 STREAMINFO header（byte 18-25）計算時長，並在回應 JSON 中一同回傳，前端快取到 `window.__mediaDurations__`。

**未解決的格式**：MP3、WAV、OGG 目前仍可能顯示 `Infinity` 時長。

### 3. M4A / AAC 無法播放

**問題**：GStreamer 播放 M4A/AAC 需要 `gstreamer1.0-plugins-bad` 或 `gstreamer1.0-libav`，多數 Linux 發行版預設未安裝。

**Workaround**：後端接收到 M4A/AAC 時，呼叫系統 `ffmpeg` 自動轉成 PCM WAV 後再提供播放。WAV 是 GStreamer 原生支援的格式。

**需求**：必須安裝 `ffmpeg`（`sudo apt install ffmpeg`）。

### 4. HTTP Range 請求支援（拖曳進度列）

**問題**：拖曳進度列需要 HTTP Range 請求（RFC 7233）；Tauri 的 `asset://` 自訂協議不支援 Range，GStreamer seek 會失效。

**Workaround**：本機 HTTP Server（`tiny_http`）自行實作 Range 請求解析與部分回應（HTTP 206）。

---

## 開發環境設定

```bash
# 安裝所有依賴
npm install

# Linux 視窗模式開發（含自訂 GTK HeaderBar）
npm run dev:linux:window

# 一般開發（所有平台）
npm run tauri dev
```

## 建置

```bash
# 建置生產版本
npm run tauri build
```

---

## 推薦 IDE

[VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)