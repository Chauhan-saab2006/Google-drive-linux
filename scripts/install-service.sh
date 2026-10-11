#!/usr/bin/env bash
# ==============================================================================
# Google Drive Linux — Systemd User Service Installer
# Installs and enables the background daemon as a systemd user unit.
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
SERVICE_NAME="google-drive-linux.service"
SERVICE_DEST="${SYSTEMD_USER_DIR}/${SERVICE_NAME}"

# Locate installed or built binary
BIN_PATH=""
if command -v google-drive-linux >/dev/null 2>&1; then
    BIN_PATH="$(command -v google-drive-linux)"
elif [[ -f "${HOME}/.local/bin/google-drive-linux" ]]; then
    BIN_PATH="${HOME}/.local/bin/google-drive-linux"
elif [[ -f "${ROOT_DIR}/target/release/google-drive-linux" ]]; then
    BIN_PATH="${ROOT_DIR}/target/release/google-drive-linux"
elif [[ -f "${ROOT_DIR}/target/debug/google-drive-linux" ]]; then
    BIN_PATH="${ROOT_DIR}/target/debug/google-drive-linux"
else
    echo "==> Binary not found. Building release binary first..."
    cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"
    BIN_PATH="${ROOT_DIR}/target/release/google-drive-linux"
fi

echo "==> Configuring systemd user service for ${BIN_PATH}..."
mkdir -p "${SYSTEMD_USER_DIR}"

cat > "${SERVICE_DEST}" <<EOF
[Unit]
Description=Google Drive Linux Virtual Filesystem Daemon
Documentation=https://github.com/your-org/google-drive-linux
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BIN_PATH} daemon
Restart=on-failure
RestartSec=10s
TimeoutStopSec=20s

[Install]
WantedBy=default.target
EOF

echo "==> Reloading systemd user daemon..."
systemctl --user daemon-reload

echo "==> Enabling and starting service..."
systemctl --user enable --now "${SERVICE_NAME}"

echo ""
echo "✓ Service successfully installed and started!"
echo "  Check status:  systemctl --user status ${SERVICE_NAME}"
echo "  View logs:     journalctl --user -u ${SERVICE_NAME} -f"
echo "  Stop service:  systemctl --user stop ${SERVICE_NAME}"
echo "  Disable:       systemctl --user disable ${SERVICE_NAME}"
