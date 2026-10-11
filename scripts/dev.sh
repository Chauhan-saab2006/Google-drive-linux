#!/usr/bin/env bash
set -euo pipefail

# Run formatting, clippy checks, and automated tests
cd "$(dirname "$0")/.."
echo "==> Checking code format..."
cargo fmt -- --check
echo "==> Running clippy linter..."
cargo clippy -- -D warnings
echo "==> Running unit tests..."
cargo test
echo "==> All development checks passed successfully!"
