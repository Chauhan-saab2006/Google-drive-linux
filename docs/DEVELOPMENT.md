# Google Drive Linux — Development Guide

## 1. Prerequisites

Ensure your development machine has the following tools and packages installed:

### On Arch / CachyOS Linux
```bash
sudo pacman -S base-devel rust cargo fuse3 gtk4 libadwaita sqlite openssl pkgconf
```

### On Ubuntu / Debian (24.04+)
```bash
sudo apt update
sudo apt install build-essential cargo rustc libfuse3-dev libgtk-4-dev libadwaita-1-dev libsqlite3-dev libssl-dev pkg-config
```

## 2. Directory Structure

```
google-drive-linux/
├── Cargo.toml          # Rust package dependencies & configuration
├── src/
│   ├── main.rs         # CLI argument handling & runtime entry point
│   ├── auth/           # Google OAuth 2.0 PKCE / token manager
│   ├── drive/          # Google Drive API v3 HTTP client
│   ├── filesystem/     # FUSE3 virtual disk implementation
│   ├── cache/          # SQLite local metadata & data caching
│   ├── sync/           # Queue manager & conflict resolution
│   ├── config/         # XDG configuration loader/persister
│   ├── daemon/         # Background service lifecycle & IPC
│   └── gui/            # GTK4 & Libadwaita desktop application
├── docs/               # Technical documentation
├── config/             # Sample config templates
├── packaging/          # Systemd unit & desktop integration
└── scripts/            # Helper automation scripts
```

## 3. Development Workflow

### Build Debug Binary
```bash
cargo build
```

### Run Tests
```bash
cargo test
```

### Linting & Formatting
Always ensure clean formatting and no clippy warnings before committing:
```bash
cargo fmt -- --check
cargo clippy -- -D warnings
```

### Run Local CLI Commands
```bash
# Print status
cargo run -- status

# Inspect configuration
cargo run -- settings --show

# Launch GUI
cargo run -- --gui
```
