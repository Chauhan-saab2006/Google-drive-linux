#!/usr/bin/env bash
# ==============================================================================
# Google Drive Linux — Package Generator
# Builds release binary and packages into:
#   1. Universal Portable Tarball (.tar.gz with install.sh/uninstall.sh)
#   2. Arch Linux / CachyOS / Manjaro Package (.pkg.tar.zst via makepkg)
#   3. Debian / Ubuntu Package (.deb)
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
PKG_VER="0.1.0"
BIN_NAME="google-drive-linux"

echo "==> Building optimized release binary..."
cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"

RELEASE_BIN="${ROOT_DIR}/target/release/${BIN_NAME}"
if [[ ! -f "${RELEASE_BIN}" ]]; then
    echo "Error: Release binary not found at ${RELEASE_BIN}" >&2
    exit 1
fi

mkdir -p "${DIST_DIR}"

# ------------------------------------------------------------------------------
# 1. Universal Standalone Tarball (.tar.gz)
# ------------------------------------------------------------------------------
echo "==> Generating Universal Linux portable bundle (.tar.gz)..."
BUNDLE_NAME="${BIN_NAME}-${PKG_VER}-x86_64"
BUNDLE_DIR="${DIST_DIR}/${BUNDLE_NAME}"
rm -rf "${BUNDLE_DIR}"
mkdir -p "${BUNDLE_DIR}"

cp "${RELEASE_BIN}" "${BUNDLE_DIR}/${BIN_NAME}"
chmod 755 "${BUNDLE_DIR}/${BIN_NAME}"
cp "${ROOT_DIR}/packaging/${BIN_NAME}.desktop" "${BUNDLE_DIR}/"
cp "${ROOT_DIR}/packaging/icons/${BIN_NAME}.svg" "${BUNDLE_DIR}/"
cp "${ROOT_DIR}/packaging/${BIN_NAME}.service" "${BUNDLE_DIR}/"

cat << 'EOF' > "${BUNDLE_DIR}/install.sh"
#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== Installing Google Drive Linux ==="

# 1. Install binary
mkdir -p "$HOME/.local/bin"
cp -f "$SCRIPT_DIR/google-drive-linux" "$HOME/.local/bin/google-drive-linux"
chmod +x "$HOME/.local/bin/google-drive-linux"
echo "✓ Installed executable to ~/.local/bin/google-drive-linux"

# Ensure ~/.local/bin is in PATH for desktop sessions if needed
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo "ℹ Note: Make sure ~/.local/bin is in your PATH."
fi

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

# 5. Refresh caches
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo ""
echo "Installation complete!"
echo "Google Drive Linux is now available in your application launcher menu."
echo "You can launch it from your app menu or terminal: google-drive-linux"
EOF
chmod +x "${BUNDLE_DIR}/install.sh"

cat << 'EOF' > "${BUNDLE_DIR}/uninstall.sh"
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
EOF
chmod +x "${BUNDLE_DIR}/uninstall.sh"

TAR_OUTPUT="${DIST_DIR}/${BIN_NAME}-${PKG_VER}-linux-x86_64.tar.gz"
tar -czf "${TAR_OUTPUT}" -C "${DIST_DIR}" "${BUNDLE_NAME}"
echo "✓ Created: ${TAR_OUTPUT}"

# ------------------------------------------------------------------------------
# 2. Arch Linux / CachyOS / Manjaro Package (.pkg.tar.zst)
# ------------------------------------------------------------------------------
if command -v makepkg >/dev/null 2>&1; then
    echo "==> Generating Arch Linux / CachyOS package (.pkg.tar.zst)..."
    ARCH_DIR="${DIST_DIR}/arch"
    mkdir -p "${ARCH_DIR}"
    cp "${RELEASE_BIN}" "${ARCH_DIR}/${BIN_NAME}"
    cp "${ROOT_DIR}/packaging/${BIN_NAME}.desktop" "${ARCH_DIR}/"
    cp "${ROOT_DIR}/packaging/icons/${BIN_NAME}.svg" "${ARCH_DIR}/"
    cp "${ROOT_DIR}/packaging/${BIN_NAME}.service" "${ARCH_DIR}/"
    cp "${ROOT_DIR}/packaging/PKGBUILD" "${ARCH_DIR}/PKGBUILD"

    # Ensure source entries match local files
    cat << 'EOF' > "${ARCH_DIR}/PKGBUILD"
# Maintainer: Google Drive Linux Contributors
pkgname=google-drive-linux-bin
pkgver=0.1.0
pkgrel=1
pkgdesc="Native Google Drive client and FUSE virtual filesystem for Linux"
arch=('x86_64')
url="https://github.com/google-drive-linux"
license=('Apache-2.0')
depends=('fuse3' 'gtk4' 'libadwaita' 'sqlite' 'openssl')
provides=('google-drive-linux')
conflicts=('google-drive-linux')
source=(
    "file://google-drive-linux"
    "file://google-drive-linux.desktop"
    "file://google-drive-linux.svg"
    "file://google-drive-linux.service"
)
sha256sums=('SKIP' 'SKIP' 'SKIP' 'SKIP')

package() {
    install -Dm755 "${srcdir}/google-drive-linux" "${pkgdir}/usr/bin/google-drive-linux"
    install -Dm644 "${srcdir}/google-drive-linux.desktop" "${pkgdir}/usr/share/applications/google-drive-linux.desktop"
    install -Dm644 "${srcdir}/google-drive-linux.svg" "${pkgdir}/usr/share/icons/hicolor/scalable/apps/google-drive-linux.svg"
    install -Dm644 "${srcdir}/google-drive-linux.service" "${pkgdir}/usr/lib/systemd/user/google-drive-linux.service"
}
EOF

    (cd "${ARCH_DIR}" && makepkg -f --nodeps)
    cp "${ARCH_DIR}"/*.pkg.tar.zst "${DIST_DIR}/" 2>/dev/null || true
    echo "✓ Created Arch package in ${DIST_DIR}/"
fi

# ------------------------------------------------------------------------------
# 3. Debian / Ubuntu Package (.deb)
# ------------------------------------------------------------------------------
if command -v ar >/dev/null 2>&1 && command -v tar >/dev/null 2>&1; then
    echo "==> Generating Debian / Ubuntu package (.deb)..."
    DEB_DIR="${DIST_DIR}/deb"
    rm -rf "${DEB_DIR}"
    mkdir -p "${DEB_DIR}/pkg/DEBIAN"
    mkdir -p "${DEB_DIR}/pkg/usr/bin"
    mkdir -p "${DEB_DIR}/pkg/usr/share/applications"
    mkdir -p "${DEB_DIR}/pkg/usr/share/icons/hicolor/scalable/apps"
    mkdir -p "${DEB_DIR}/pkg/usr/lib/systemd/user"

    cat << EOF > "${DEB_DIR}/pkg/DEBIAN/control"
Package: ${BIN_NAME}
Version: ${PKG_VER}
Section: utils
Priority: optional
Architecture: amd64
Depends: fuse3, libgtk-4-1, libadwaita-1-0, libsqlite3-0, libssl3
Maintainer: Google Drive Linux Contributors <noreply@example.com>
Description: Native Google Drive client and FUSE virtual filesystem for Linux
 Mount and access your Google Drive as a local disk with GTK4 GUI.
EOF

    cp "${RELEASE_BIN}" "${DEB_DIR}/pkg/usr/bin/${BIN_NAME}"
    chmod 755 "${DEB_DIR}/pkg/usr/bin/${BIN_NAME}"
    cp "${ROOT_DIR}/packaging/${BIN_NAME}.desktop" "${DEB_DIR}/pkg/usr/share/applications/"
    chmod 644 "${DEB_DIR}/pkg/usr/share/applications/${BIN_NAME}.desktop"
    cp "${ROOT_DIR}/packaging/icons/${BIN_NAME}.svg" "${DEB_DIR}/pkg/usr/share/icons/hicolor/scalable/apps/"
    chmod 644 "${DEB_DIR}/pkg/usr/share/icons/hicolor/scalable/apps/${BIN_NAME}.svg"
    cp "${ROOT_DIR}/packaging/${BIN_NAME}.service" "${DEB_DIR}/pkg/usr/lib/systemd/user/"
    chmod 644 "${DEB_DIR}/pkg/usr/lib/systemd/user/${BIN_NAME}.service"

    (
        cd "${DEB_DIR}"
        echo "2.0" > debian-binary
        tar --numeric-owner --group=0 --owner=0 -czf control.tar.gz -C pkg/DEBIAN .
        tar --numeric-owner --group=0 --owner=0 -czf data.tar.gz -C pkg usr
        ar rcs "${DIST_DIR}/${BIN_NAME}_${PKG_VER}_amd64.deb" debian-binary control.tar.gz data.tar.gz
        rm -rf "${DEB_DIR}"
    )
    echo "✓ Created Debian package: ${DIST_DIR}/${BIN_NAME}_${PKG_VER}_amd64.deb"
fi

echo ""
echo "=== All packages built successfully in ${DIST_DIR} ==="
ls -lh "${DIST_DIR}"/*.tar.gz "${DIST_DIR}"/*.pkg.tar.zst "${DIST_DIR}"/*.deb 2>/dev/null || true
