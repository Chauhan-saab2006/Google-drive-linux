#!/usr/bin/env bash
set -euo pipefail

echo "=== Uninstalling Google Drive Linux ==="

# Stop systemd service if running
systemctl --user stop google-drive-linux.service 2>/dev/null || true
systemctl --user disable google-drive-linux.service 2>/dev/null || true

# Remove installed files
rm -f "$HOME/.local/bin/google-drive-linux"
rm -f "$HOME/.local/share/icons/hicolor/scalable/apps/google-drive-linux.svg"
rm -f "$HOME/.local/share/applications/google-drive-linux.desktop"
rm -f "$HOME/.config/systemd/user/google-drive-linux.service"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$HOME/.local/share/applications" || true
fi

echo "✓ Google Drive Linux has been uninstalled."
