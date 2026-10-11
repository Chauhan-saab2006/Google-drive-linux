# Google Drive Linux — FUSE Virtual Filesystem Specification

## 1. Architecture Overview

Google Drive Linux exposes Google Drive as a real POSIX virtual disk using **FUSE3** via the `fuser` crate.
Applications on Linux (such as GNOME Nautilus, KDE Dolphin, Thunar, `cat`, `ls`, `ffmpeg`, etc.) interact with the virtual filesystem at `~/GoogleDrive` exactly like local storage.

```
+-------------------------------------------------------------+
|    Linux Desktop Applications (Nautilus / Dolphin / CLI)     |
+------------------------------+------------------------------+
                               | POSIX System Calls (open, read, readdir, getattr)
                               v
+-------------------------------------------------------------+
|                     Linux Kernel (FUSE)                     |
+------------------------------+------------------------------+
                               | /dev/fuse
                               v
+-------------------------------------------------------------+
|       Google Drive Linux Daemon (GoogleDriveFS / fuser)     |
|                                                             |
|   +-------------------+              +------------------+   |
|   |    InodeTable     |              | Range Streaming  |   |
|   |  ino <-> drive_id |              | HTTP Range: byte |   |
|   +---------+---------+              +--------+---------+   |
+-------------|---------------------------------|-------------+
              |                                 |
              v                                 v
+-------------------------------------------------------------+
|                   Google Drive API v3                       |
|   https://www.googleapis.com/drive/v3/files                 |
+-------------------------------------------------------------+
```

---

## 2. Inode Management & Mapping

Google Drive identifies all files and folders by alphanumeric string IDs (e.g. `1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OIvE2up0Y`). POSIX filesystems require numeric 64-bit inodes (`u64` / `INodeNo`).

### Inode Allocation Rules:
1. **Root Inode (`1` / `INodeNo::ROOT`)**:
   - Mapped to Google Drive's special `"root"` folder.
   - Always represents the mount directory `~/GoogleDrive`.
2. **Child Inodes (`2..2^64-1`)**:
   - Allocated dynamically via atomic monotonically increasing sequence.
   - Maintained in `InodeTable` with bidirectional lookups:
     - `ino -> InodeEntry`
     - `drive_id -> ino`
     - `(parent_ino, child_name) -> child_ino`
     - `parent_ino -> Vec<child_ino>`

---

## 3. Implemented POSIX Operations

| Operation | FUSE Callback | Behavior |
|-----------|---------------|----------|
| `lookup` | `lookup(parent, name)` | Resolves file or directory by name. Queries Google Drive API if not cached in memory. Returns `FileAttr` with TTL. |
| `getattr` | `getattr(ino)` | Returns POSIX attributes (`0o755` for folders, `0o644` for files, size in bytes, timestamps). |
| `readdir` | `readdir(ino, offset)` | Returns `.` and `..` followed by children. Lazily queries Google Drive API for children of `drive_id`. |
| `open` | `open(ino, flags)` | Checks permissions and flags. In read-only mode, rejects write flags (`O_WRONLY`, `O_RDWR`) with `EROFS`. |
| `read` | `read(ino, offset, size)` | Streams byte chunks on-demand from Google Drive using HTTP `Range: bytes=start-end`. Zero unnecessary RAM or disk usage. |

---

## 4. Mounting & Unmounting

### Mounting:
```bash
# Mount at default ~/GoogleDrive
google-drive-linux mount

# Or mount at custom directory
google-drive-linux mount --target /mnt/my-drive
```

### Unmounting:
```bash
# Graceful unmount
google-drive-linux unmount

# Or standard Linux command
fusermount3 -u ~/GoogleDrive
```

### Graphical Mount:
Users can also toggle **Mount** / **Unmount** directly from the **Google Drive Linux** desktop application window.
