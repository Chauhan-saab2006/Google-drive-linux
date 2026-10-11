# Google Drive Linux

**Google Drive Linux** is a native Linux desktop application and virtual filesystem that mounts your Google Drive as a local directory (e.g., `~/GoogleDrive`), providing an experience similar to Google Drive for Desktop on Windows and macOS.

Built with **Rust**, **GTK4 / Libadwaita**, and **FUSE3**, it integrates smoothly with GNOME (Nautilus), KDE (Dolphin), XFCE (Thunar), and other Linux desktop environments.

---

## Key Highlights

- **Native FUSE3 Filesystem**: Mount Google Drive directly into your home directory (`~/GoogleDrive`) without downloading your entire Drive to local disk.
- **On-Demand Streaming & Lazy Loading**: Files and directory trees are fetched dynamically upon access, saving bandwidth and local storage.
- **Local SQLite Metadata Cache**: High-speed browsing with local inode and attribute caching.
- **Write Buffering & Offline Queue**: Edit, create, rename, and delete files locally with an intelligent background upload queue that gracefully handles temporary network interruptions.
- **Modern Libadwaita GUI**: Clean, responsive interface adhering to GNOME desktop HIG for account management, mount status, and synchronization preferences.
- **Full-featured CLI**: Manage headless or server setups using commands like `google-drive-linux status`, `mount`, `unmount`, `login`, and `daemon`.
- **Systemd User Service**: Automatic mounting and daemon background management across reboots.

---

## Architecture Overview

```
Google Drive API (v3)
       │
       ▼
Google Drive Client (Rust / reqwest / async)
       │
       ▼
Sync & Local SQLite Cache Engine (~/.cache/google-drive-linux/)
       │
       ▼
POSIX / FUSE3 Virtual Filesystem
       │
       ▼
Mount Point (~/GoogleDrive)
       │
       ▼
Linux File Managers (Nautilus, Dolphin, Thunar) & Native Apps
```

---

## Development Milestones

- [x] **Milestone 1**: Project Foundation (Architecture, Modular Structure, Configuration, CLI & Libadwaita GUI skeletons)
- [x] **Milestone 2**: Google OAuth 2.0 (RFC 7636 PKCE flow, loopback server, secure 0600 token storage, auto-refresh, logout)
- [x] **Milestone 3**: Google Drive API v3 Client (Metadata queries, search, upload, download, range-based streaming, pagination, backoff retries)
- [x] **Milestone 4**: Read-Only FUSE3 Filesystem (Inode table, directory listing, metadata lookup, range-based file streaming)
- [x] **Milestone 5**: Write Support (File creation, modification, rename, move, delete, and Google Workspace stub detection)
- [x] **Milestone 6**: Local SQLite Metadata & Chunk Cache (WAL mode, fast directory cache, and LRU eviction)
- [x] **Milestone 7**: Background Daemon & Systemd User Service (Heartbeat loop, auto-reconnect, and unmount cleanup)
- [x] **Milestone 8**: Remote Change Synchronization & Offline Queue (Google Drive Changes API v3 & persistent write queue)
- [x] **Milestone 9**: Desktop Application Integration (XDG .desktop entry, custom SVG vector icon, and installer scripts)
- [x] **Milestone 10**: Test Suite & Verification (43 automated tests across all modules)

---

## Prerequisites

- **Linux Kernel**: 5.15+ with FUSE support
- **FUSE3**: `libfuse3-dev` / `fuse3`
- **GTK4 & Libadwaita**: `gtk4` >= 4.12, `libadwaita-1` >= 1.4
- **Rust Toolchain**: `rustc` 1.80+ / `cargo`
- **SQLite3 & OpenSSL**: `sqlite3`, `openssl`

---

## Quick Start

### 1. Build & Test
```bash
cargo build
cargo test
```

### 2. Configure OAuth Credentials (Interactive Wizard)
```bash
./scripts/setup-oauth.sh ./target/debug/google-drive-linux
```

### 3. Install Desktop Integration & Launcher
```bash
./scripts/install-desktop.sh
```
This installs the binary to `~/.local/bin`, the desktop launcher to `~/.local/share/applications`, and the SVG icon to your desktop icon theme.

### 4. Enable Background Auto-Mount Daemon (Systemd)
```bash
./scripts/install-service.sh
```

### 5. CLI Commands
```bash
google-drive-linux status           # View account, token, quota, and mount status
google-drive-linux mount            # Mount Google Drive to ~/GoogleDrive
google-drive-linux unmount          # Unmount Google Drive
google-drive-linux list             # List files in Google Drive root
google-drive-linux sync             # Manually trigger offline flush & remote change poll
google-drive-linux daemon           # Run daemon in foreground
google-drive-linux                  # Launch Libadwaita desktop GUI (default)
```

---

## Documentation

Detailed architectural and developer guides are available in the [`docs/`](docs/) directory:
- [Architecture](docs/ARCHITECTURE.md)
- [Development Guide](docs/DEVELOPMENT.md)
- [Google OAuth 2.0 Security](docs/GOOGLE_OAUTH.md)
- [FUSE Virtual Filesystem](docs/FILESYSTEM.md)
- [Security Model](docs/SECURITY.md)
- [Installation](docs/INSTALLATION.md)
- [Troubleshooting](docs/TROUBLESHOOTING.md)

---

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
