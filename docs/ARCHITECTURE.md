# Google Drive Linux — System Architecture

## 1. High-Level Design

Google Drive Linux exposes remote Google Drive objects as a virtual POSIX filesystem on Linux, adhering to modern desktop standards.

```
┌────────────────────────────────────────────────────────┐
│                   Desktop User Space                   │
│  Nautilus / Dolphin / Thunar / Bash / Native Apps      │
└─────────────────────────┬──────────────────────────────┘
                          │ POSIX System Calls (open, read, write, rename...)
                          ▼
┌────────────────────────────────────────────────────────┐
│                   Kernel FUSE Driver                   │
└─────────────────────────┬──────────────────────────────┘
                          │ /dev/fuse
                          ▼
┌────────────────────────────────────────────────────────┐
│               google-drive-linux Backend               │
│                                                        │
│  ┌────────────────────┐      ┌──────────────────────┐  │
│  │  FUSE3 Filesystem  │◄────►│  POSIX/Drive Bridge  │  │
│  └─────────┬──────────┘      └──────────┬───────────┘  │
│            │ Inode Lookup               │              │
│            ▼                            ▼              │
│  ┌────────────────────┐      ┌──────────────────────┐  │
│  │ Local SQLite Cache │      │ Background Sync      │  │
│  │ (Metadata & Inode) │      │ & Upload Queue       │  │
│  └────────────────────┘      └──────────┬───────────┘  │
│                                         │              │
│  ┌────────────────────┐                 ▼              │
│  │ Libadwaita / GTK4  │      ┌──────────────────────┐  │
│  │ GUI & Status Window│      │ Google Drive API v3  │  │
│  └────────────────────┘      └──────────┬───────────┘  │
└─────────────────────────────────────────┼──────────────┘
                                          │ HTTPS / OAuth 2.0
                                          ▼
                               ┌──────────────────────┐
                               │ Google Cloud Backend │
                               └──────────────────────┘
```

## 2. Core Subsystems

### 2.1 Virtual Filesystem (FUSE3)
- Implemented via FUSE 3 userspace API (`fuser` crate).
- Maintains a bidirectional map between numerical 64-bit POSIX Inodes and Google Drive opaque string IDs (`drive_id`).
- Supports lazy directory evaluation: `readdir` queries the local SQLite cache first, making incremental API queries only when needed.

### 2.2 Google Drive Translation Layer
- Google Drive is object-based, allows duplicate filenames in the same folder, and has non-binary files (e.g. Google Docs, Sheets, Slides).
- The translation layer maps:
  - POSIX directory hierarchy to Google Drive `parents` relationships.
  - POSIX file writes into local disk temporary buffers before triggering resumable Drive API uploads.
  - Native Google Docs to desktop URL shortcuts (`.desktop` files or opening default browser on read).

### 2.3 Local Cache & Metadata Storage (SQLite)
- Stored under `$XDG_CACHE_HOME/google-drive-linux/meta.db`.
- Indexed tables:
  - `inodes`: Maps `ino` <-> `drive_id`, `parent_ino`, `name`, `mime_type`, `size`, `mtime`.
  - `file_cache`: Tracks local cached chunks and data files.
  - `upload_queue`: Persists uncommitted local modifications and ensures zero data loss during network interruptions.

### 2.4 Desktop Interface (GTK4 & Libadwaita)
- Modern GNOME desktop design principles.
- Displays account status, link state, mount controls, storage usage, and auto-start configuration.
- Connects to the background daemon via local IPC (D-Bus or UNIX domain socket).
