# PaheFlow

<p align="center">
  <img src="static/icon.png" width="128" height="128" alt="PaheFlow Icon" />
</p>

<p align="center">
  <strong>Fast, lightweight native AnimePahe stream extractor and concurrent downloader for Android & Linux.</strong>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-GPLv3-blue.svg" alt="License: GPLv3" /></a>
  <img src="https://img.shields.io/badge/Rust-1.75+-orange.svg?logo=rust" alt="Rust Version" />
  <img src="https://img.shields.io/badge/Platform-Android%20%7C%20Linux-success.svg" alt="Platform" />
  <img src="https://img.shields.io/badge/Architecture-ARM64%20%7C%20x86__64-informational.svg" alt="Architecture" />
</p>

---

## Overview

**PaheFlow** is a modern, high-performance streaming client and batch downloader built specifically for AnimePahe. Designed from the ground up for speed, reliability, and cross-platform flexibility, PaheFlow pairs a native **Rust core** (using Tokio, Axum, and Reqwest) with a sleek, responsive Web interface and an Android native application.

Whether running as an APK on your phone or as a lightweight desktop service on Linux, PaheFlow eliminates the friction of streaming and downloading: it bypasses Cloudflare hurdles, extracts HLS playlists directly, and streams concurrent fragments with hardware-accelerated AES-128 decryption.

---

## Key Features

- ⚡ **High-Speed Multi-Fragment Downloader**: Download episodes using 1–64 concurrent HLS segment streams with automated chunk retries, exponential backoff, rate-limit throttling, and seamless MP4 remuxing.
- 📱 **Native Android Application**:
  - Embedded WebView UI bridged directly to the high-performance Rust core via JNI.
  - **In-App Cloudflare Solver**: Solves Cloudflare Turnstile/clearance challenges seamlessly inside an in-app bottom sheet—no external browser or manual cookie copying needed.
  - **Scoped Storage Resilient**: Automated POSIX writeability probes dynamically fall back from `Download/PaheFlow` to app-specific external or sandbox storage, completely eliminating `os error 1 (Operation not permitted)` crashes on Android 11+.
  - In-app storage permission requests and status banners.
- 🎧 **Audio & Resolution Filtering**:
  - Filter releases instantly by audio language: **Sub**, **Dub**, or **All**.
  - Choose resolutions: 720p, 1080p, or 360p.
  - View real-time **episode file sizes** (MB/GB) directly on episode badges.
- 🎨 **Modern Responsive UI**:
  - Six refined colour themes: *Midnight* (default), *Daylight*, *Ink*, *Sepia*, *Nord*, and *Rose*.
  - Real-time Server-Sent Events (SSE) log stream, progress bars, download speed, and ETA calculations.
  - One-tap episode range selectors (`1-12`, `13-24`, select all, invert).
- 🛡️ **Robust Networking & Security**:
  - Loopback-only binding (`127.0.0.1`), strict `Host` header enforcement to prevent DNS-rebinding attacks, and per-session security token authorization (`X-App-Token`).
  - Automated DNS resolution fallbacks for AnimePahe and video CDN endpoints.
  - Companion browser extension (`tools/extension`) for 1-click Cloudflare cookie synchronization on desktop.
- 📚 **Library & File Manager**:
  - Built-in video library with safe deletion guards, folder organization, and direct playback launching.

---

## Architecture

```
                  ┌───────────────────────────────────────────┐
                  │            PaheFlow UI Layer              │
                  │  (Vanilla JS, SSE, Modern CSS Themes)     │
                  └──────────────┬────────────────────────────┘
                                 │ HTTP / SSE / REST API
                                 ▼
┌─────────────────────────────────────────────────────────────┐
│                      Rust Native Core                       │
│  ┌───────────────────────┐       ┌───────────────────────┐  │
│  │   Axum / Tokio API    │◄─────►│    Runner & Engine    │  │
│  └───────────────────────┘       └───────────┬───────────┘  │
│                                              ▼              │
│  ┌───────────────────────┐       ┌───────────────────────┐  │
│  │ Kwik / M3U8 Extractor │       │  HLS Fragment Engine  │  │
│  │   & Dean Edwards      │       │ (AES-128 In-Flight)   │  │
│  └───────────────────────┘       └───────────────────────┘  │
└──────────────────────────────┬──────────────────────────────┘
                               │ JNI Interface (android.rs)
                               ▼
┌─────────────────────────────────────────────────────────────┐
│                     Android App (Kotlin)                    │
│  • Cloudflare BottomSheet Solver  • Scoped Storage Bridge   │
│  • Foreground Service Watcher     • App Sandbox Manager     │
└─────────────────────────────────────────────────────────────┘
```

---

## Getting Started

### Android (APK)

1. Grab the latest `paheflow-v1.0.1.apk` from the Releases page or build from source below.
2. Install the APK on your device (Android 8.0+ / API 26+).
3. Grant storage permissions when prompted so downloads can be saved directly to `Download/PaheFlow`.
4. If prompted with a Cloudflare notice, tap **"Solve Cloudflare"**, solve the challenge in the sheet, and resume downloading immediately.

### Linux Desktop

#### Prerequisites
- **Rust 1.75+** (`cargo` and `rustup`)
- **ffmpeg** (for remuxing streams into standard MP4)

#### 1. Quick Launch
```bash
# Clone the repository
git clone <repository-url>
cd animepahe_gui

# Run dependency check / setup
./requirements.sh

# Build release and launch
./animepahe-gui
```
The launcher starts the local server on `http://127.0.0.1:8765/` and opens your default browser.

#### 2. System Desktop Integration
To install PaheFlow into your desktop application menu (`~/.local/share/applications`):
```bash
./install-desktop.sh
```

#### 3. Tauri Desktop Shell (Optional)
If you prefer a standalone desktop window without launching an external browser:
```bash
cargo run --manifest-path src-tauri/Cargo.toml --release
```

---

## Configuration & Storage

Settings are saved in `config.json` (created automatically with restricted `0600` permissions):

```json
{
  "cf": "cf_clearance_cookie_value",
  "ua": "Mozilla/5.0 ...",
  "concurrent_fragments": 8,
  "download_dir": "/path/to/downloads"
}
```

### Storage Fallback Hierarchy (Android)
On Android 11+, system Scoped Storage can prevent arbitrary file creation under `/storage/emulated/0/Download`. PaheFlow utilizes a 3-tier fallback strategy:
1. **Primary**: `/storage/emulated/0/Download/PaheFlow` (Standard user Downloads folder).
2. **App-Specific External**: `/storage/emulated/0/Android/data/com.animepahe.app/files/Download` (Accessible by file managers, unrestricted write access).
3. **App Sandbox**: Internal app data directory.

### Environment Variables

| Variable | Description | Default |
|---|---|---|
| `ANIMEPAHE_GUI_PORT` | Custom port for local HTTP server | `8765` |
| `ANIMEPAHE_GUI_NO_BROWSER` | Set to `1` to prevent auto-opening browser | `0` |
| `ANIMEPAHE_GUI_RESOURCES` | Path to static assets (`static/` and `anime.list`) | Current dir / App dir |
| `ANIMEPAHE_GUI_DATA_DIR` | Path to store writeable cache and `config.json` | Local app directory |

---

## Building from Source

### Compiling Rust Engine & Android JNI Libraries
Make sure the Android NDK and `cargo-ndk` are installed:
```bash
cargo install cargo-ndk
rustup target add aarch64-linux-android x86_64-linux-android

# Build native libraries with symbol stripping
RUSTFLAGS="--remap-path-prefix=$HOME=/workspace" cargo ndk \
  -t arm64-v8a -t x86_64 \
  -o android/app/src/main/jniLibs \
  build --release
```

### Assembling the Release APK
```bash
cd android
./gradlew assembleRelease
# The release APK will be generated at android/app/build/outputs/apk/release/app-release.apk
```

---

## Testing & CI/CD

PaheFlow includes automated test suites covering all layers of the application:

```bash
# 1. Rust unit & integration tests
cargo test

# 2. Rust formatting & clippy linter
cargo fmt --all --check
cargo clippy -- -D warnings

# 3. Python backend regression tests
python3 -m unittest discover -s tests -v
```

GitHub Actions automatically runs the full test matrix on every push and pull request via `.github/workflows/ci.yml`.

---

## License

This project is licensed under the **GNU General Public License v3.0 (GPL-3.0)**. See the [LICENSE](LICENSE) file for the full text.

### Third-Party Attributions
- The legacy helper script `animepahe-dl.sh` is based on [KevCui/animepahe-dl](https://github.com/KevCui/animepahe-dl) and remains under its original WTFPL license.
