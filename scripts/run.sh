#!/usr/bin/env bash
set -euo pipefail

# Build and execute the Google Drive Linux GUI
cd "$(dirname "$0")/.."
cargo run -- --gui "$@"
