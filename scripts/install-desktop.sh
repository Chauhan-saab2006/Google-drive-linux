#!/usr/bin/env bash
# ==============================================================================
# Google Drive Linux — Desktop Integration Installer
# Installs binary, desktop launcher, and application icon into user space.
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

BIN_NAME="google-drive-linux"
TARGET_DIR="${ROOT_DIR}/target/release"
BINARY="${TARGET_DIR}/${BIN_NAME}"

if [[ ! -f "${BINARY}" ]]; then
    # Fallback to debug binary if release build not yet done
    if [[ -f "${ROOT_DIR}/target/debug/${BIN_NAME}" ]]; then
        BINARY="${ROOT_DIR}/target/debug/${BIN_NAME}"
    else
        echo "==> Building ${BIN_NAME} in release mode..."
        cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"
        BINARY="${ROOT_DIR}/target/release/${BIN_NAME}"
    fi
fi

# Standard XDG user directories
BIN_DEST="${HOME}/.local/bin"
APP_DEST="${HOME}/.local/share/applications"
ICON_DEST="${HOME}/.local/share/icons/hicolor/scalable/apps"

echo "==> Installing Google Drive Linux for user $(whoami)..."

mkdir -p "${BIN_DEST}"
mkdir -p "${APP_DEST}"
mkdir -p "${ICON_DEST}"

# 1. Install binary
echo "  -> Copying binary to ${BIN_DEST}/${BIN_NAME}"
install -m 755 "${BINARY}" "${BIN_DEST}/${BIN_NAME}"

# 2. Install icon
echo "  -> Installing icon to ${ICON_DEST}/${BIN_NAME}.svg"
install -m 644 "${ROOT_DIR}/packaging/icons/${BIN_NAME}.svg" "${ICON_DEST}/${BIN_NAME}.svg"

# 3. Install desktop entry
echo "  -> Installing desktop entry to ${APP_DEST}/${BIN_NAME}.desktop"
install -m 644 "${ROOT_DIR}/packaging/${BIN_NAME}.desktop" "${APP_DEST}/${BIN_NAME}.desktop"

# 4. Refresh desktop and icon caches
if command -v update-desktop-database >/dev/null 2>&1; then
    echo "  -> Updating desktop database..."
    update-desktop-database "${APP_DEST}" || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    echo "  -> Updating icon cache..."
    gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo ""
echo "✓ Installation successful!"
echo "  You can now launch 'Google Drive' from your GNOME/KDE/XFCE application launcher."
echo "  Make sure '${HOME}/.local/bin' is in your PATH."
