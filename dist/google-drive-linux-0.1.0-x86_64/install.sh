#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== Installing Google Drive Linux ==="

# 1. Install binary
mkdir -p "$HOME/.local/bin"
cp -f "$SCRIPT_DIR/google-drive-linux" "$HOME/.local/bin/google-drive-linux"
chmod +x "$HOME/.local/bin/google-drive-linux"
echo "✓ Installed executable to ~/.local/bin/google-drive-linux"

# 2. Install icon
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
mkdir -p "$ICON_DIR"
cp -f "$SCRIPT_DIR/google-drive-linux.svg" "$ICON_DIR/google-drive-linux.svg"
echo "✓ Installed application icon"

# 3. Install desktop launcher
DESKTOP_DIR="$HOME/.local/share/applications"
mkdir -p "$DESKTOP_DIR"
cp -f "$SCRIPT_DIR/google-drive-linux.desktop" "$DESKTOP_DIR/google-drive-linux.desktop"
echo "✓ Installed desktop launcher to ~/.local/share/applications/google-drive-linux.desktop"

# 4. Install systemd user service
SERVICE_DIR="$HOME/.config/systemd/user"
mkdir -p "$SERVICE_DIR"
cp -f "$SCRIPT_DIR/google-drive-linux.service" "$SERVICE_DIR/google-drive-linux.service"
echo "✓ Installed systemd service to ~/.config/systemd/user/google-drive-linux.service"

# 5. Update desktop database
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo ""
echo "Installation complete!"
echo "Google Drive Linux is now available in your application launcher menu."
echo "You can also run it from your terminal: google-drive-linux"
