#!/usr/bin/env bash
# setup-oauth.sh — Interactive helper to configure Google OAuth 2.0 credentials
# for Google Drive Linux.
#
# Usage:
#   chmod +x scripts/setup-oauth.sh
#   ./scripts/setup-oauth.sh

set -euo pipefail

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/google-drive-linux"
CONFIG_FILE="$CONFIG_DIR/config.toml"
BINARY="${1:-./target/debug/google-drive-linux}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

header() {
    echo
    echo -e "${BOLD}${CYAN}═══════════════════════════════════════════════${RESET}"
    echo -e "${BOLD}${CYAN}   Google Drive Linux — OAuth Setup Wizard      ${RESET}"
    echo -e "${BOLD}${CYAN}═══════════════════════════════════════════════${RESET}"
    echo
}

step() {
    echo -e "${YELLOW}▶ $1${RESET}"
}

ok() {
    echo -e "${GREEN}✓ $1${RESET}"
}

err() {
    echo -e "${RED}✗ $1${RESET}"
}

header

echo "This script will help you configure your Google Cloud OAuth 2.0"
echo "credentials so Google Drive Linux can access your Google Drive."
echo

step "Step 1 — Create a Google Cloud Project"
echo "  1. Open: https://console.cloud.google.com/"
echo "  2. Create a new project (e.g. 'Google Drive Linux')"
echo "  3. Navigate to: APIs & Services → Library"
echo "  4. Search for 'Google Drive API' and click Enable"
echo

read -rp "Press ENTER when you have enabled the Google Drive API..."
echo

step "Step 2 — Configure OAuth Consent Screen"
echo "  1. Go to: APIs & Services → OAuth consent screen"
echo "  2. Select User Type: External → Create"
echo "  3. App name: 'Google Drive Linux'"
echo "  4. Add your email as developer contact"
echo "  5. Under Scopes, add:"
echo "     - .../auth/drive"
echo "     - .../auth/userinfo.email"
echo "     - .../auth/userinfo.profile"
echo "  6. Under Test users, add YOUR Google account email"
echo "  7. Save and continue"
echo

read -rp "Press ENTER when you have completed the consent screen setup..."
echo

step "Step 3 — Create OAuth 2.0 Client ID"
echo "  1. Go to: APIs & Services → Credentials"
echo "  2. Click: + Create Credentials → OAuth client ID"
echo "  3. Application type: Desktop app"
echo "  4. Name: 'Google Drive Linux Desktop'"
echo "  5. Click Create"
echo "  6. Copy your Client ID and Client Secret"
echo

read -rp "Enter your Client ID: " CLIENT_ID
CLIENT_ID="${CLIENT_ID// /}"

if [[ -z "$CLIENT_ID" ]]; then
    err "Client ID cannot be empty. Exiting."
    exit 1
fi

read -rp "Enter your Client Secret (optional, leave blank if not provided): " CLIENT_SECRET
CLIENT_SECRET="${CLIENT_SECRET// /}"

step "Step 4 — Writing configuration"
mkdir -p "$CONFIG_DIR"

if [[ -f "$CONFIG_FILE" ]]; then
    # Update existing config
    # Remove old client_id / client_secret lines
    grep -v '^client_id\s*=' "$CONFIG_FILE" | grep -v '^client_secret\s*=' > "$CONFIG_FILE.tmp" || true
    mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
else
    # Create new config from defaults
    cat > "$CONFIG_FILE" << 'TOML'
mount_point = "~/GoogleDrive"
cache_dir = "~/.cache/google-drive-linux"
cache_size_mb = 5120
auto_start = false
auto_mount = true
log_level = "info"
TOML
fi

echo "client_id = \"$CLIENT_ID\"" >> "$CONFIG_FILE"
if [[ -n "$CLIENT_SECRET" ]]; then
    echo "client_secret = \"$CLIENT_SECRET\"" >> "$CONFIG_FILE"
fi

ok "Configuration written to: $CONFIG_FILE"
echo

step "Step 5 — Running OAuth authorization flow"
echo "  Your browser will open automatically."
echo "  Sign in with your Google account and click Allow."
echo

if [[ ! -x "$BINARY" ]]; then
    err "Binary not found at: $BINARY"
    echo "  Please build the project first:"
    echo "    cd /run/media/apma/New\\ Volume/Data/DriveMount"
    echo "    cargo build"
    exit 1
fi

"$BINARY" login --client-id "$CLIENT_ID" ${CLIENT_SECRET:+--client-secret "$CLIENT_SECRET"}
echo

step "Step 6 — Verify authentication"
"$BINARY" status
echo

step "Step 7 — Mount Google Drive"
mkdir -p "$HOME/GoogleDrive"

echo -e "${BOLD}To mount your Google Drive, run:${RESET}"
echo "  $BINARY mount"
echo
echo -e "${BOLD}To browse your files:${RESET}"
echo "  ls ~/GoogleDrive"
echo "  nautilus ~/GoogleDrive &   # GNOME"
echo "  dolphin ~/GoogleDrive &    # KDE"
echo
echo -e "${BOLD}To launch the GUI:${RESET}"
echo "  $BINARY"
echo

ok "Setup complete!"
