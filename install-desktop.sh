#!/usr/bin/env bash
# AnimePahe GUI — Desktop Application Installer for Linux
# Integrates AnimePahe GUI directly into your desktop environment (GNOME, KDE Plasma, XFCE, etc.)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_SRC="$SCRIPT_DIR/target/release/animepahe-gui"
BIN_DEST="${HOME}/.local/bin/animepahe-gui"
APPS_DIR="${HOME}/.local/share/applications"
ICONS_DIR="${HOME}/.local/share/icons/hicolor"
DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
RESOURCE_DIR="${DATA_HOME}/animepahe-gui"

echo "=================================================="
echo "  AnimePahe GUI — Linux Desktop Integration"
echo "=================================================="

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo was not found. Install Rust from https://rustup.rs and retry." >&2
    exit 1
fi

# 1. Build release binary
echo "[1/5] Building optimized Rust binary..."
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

# 2. Install binary to ~/.local/bin
echo "[2/5] Stopping any running instance and installing binary to $BIN_DEST..."
systemctl --user stop "app-animepahe*" 2>/dev/null || true
pkill -x animepahe-gui 2>/dev/null || true
sleep 0.5
mkdir -p "$(dirname "$BIN_DEST")"
cp -f "$BIN_SRC" "$BIN_DEST"
chmod +x "$BIN_DEST"

# 3. Install read-only web resources into the XDG data directory. The binary
#    finds them through ANIMEPAHE_GUI_RESOURCES set by the launcher below.
echo "[3/5] Installing web resources to $RESOURCE_DIR..."
mkdir -p "$RESOURCE_DIR"
rm -rf "$RESOURCE_DIR/static"
cp -a "$SCRIPT_DIR/static" "$RESOURCE_DIR/static"
cp -f "$SCRIPT_DIR/anime.list" "$RESOURCE_DIR/anime.list"

# Make the Cloudflare helper reachable from the installed launcher. The app
# checks ~/.local/bin explicitly, so a symlink is enough and future
# requirements.sh updates still apply.
if ! command -v curl-impersonate >/dev/null 2>&1 \
    && ! command -v animepahe-curl >/dev/null 2>&1; then
    if [[ -x "$SCRIPT_DIR/.uv/bin/animepahe-curl" ]]; then
        ln -sf "$SCRIPT_DIR/.uv/bin/animepahe-curl" "$HOME/.local/bin/animepahe-curl"
        echo "      linked Cloudflare helper into $HOME/.local/bin"
    else
        echo "warning: no curl-impersonate or animepahe-curl found." >&2
        echo "         Run ./requirements.sh before installing the desktop entry." >&2
    fi
fi

# 4. Install icons
echo "[4/5] Installing application icons..."
mkdir -p "$ICONS_DIR/scalable/apps" "$ICONS_DIR/256x256/apps"
cp -f "$SCRIPT_DIR/static/icon.svg" "$ICONS_DIR/scalable/apps/animepahe-gui.svg"
cp -f "$SCRIPT_DIR/static/icon.png" "$ICONS_DIR/256x256/apps/animepahe-gui.png"

# 5. Install .desktop file
echo "[5/5] Registering desktop application shortcut..."
mkdir -p "$APPS_DIR"
cat > "$APPS_DIR/animepahe-gui.desktop" << EOF
[Desktop Entry]
Name=AnimePahe GUI
GenericName=Anime Downloader
Comment=Native AnimePahe Downloader and Library Manager
Exec=env "ANIMEPAHE_GUI_RESOURCES=$RESOURCE_DIR" "$BIN_DEST"
Icon=animepahe-gui
Terminal=false
Type=Application
Categories=AudioVideo;Video;Network;
StartupNotify=true
Keywords=anime;animepahe;download;hls;stream;
EOF

chmod +x "$APPS_DIR/animepahe-gui.desktop"

# Update desktop databases if tools are present
update-desktop-database "$APPS_DIR" 2>/dev/null || true
gtk-update-icon-cache -f -t "$ICONS_DIR" 2>/dev/null || true
systemctl --user daemon-reload 2>/dev/null || true

echo ""
echo "✅ Installation Complete!"
echo "You can now launch 'AnimePahe GUI' directly from your application launcher / start menu."
echo "Or run from terminal: ANIMEPAHE_GUI_RESOURCES=\"$RESOURCE_DIR\" \"$BIN_DEST\""
echo "=================================================="
