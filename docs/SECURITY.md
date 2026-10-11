# Google Drive Linux — Security Model

## 1. Core Principles

- **No Passwords**: The application never prompts for, transmits, or stores user Google passwords.
- **Least Privilege**: Only scopes required for file synchronization (`https://www.googleapis.com/auth/drive`) are requested.
- **Secret Redaction**: Access tokens, refresh tokens, and sensitive headers are strictly filtered out of log outputs and crash reports.
- **Local Isolation**: Cached files and tokens are stored in user-owned directories (`0700` directory permissions, `0600` file permissions).
- **Transport Security**: All communication with Google Drive APIs occurs exclusively over encrypted HTTPS (TLS 1.3/1.2).
- **No Root Privileges**: The daemon and FUSE mounting run entirely within normal user permissions (`fusermount3` setuid handling).

## 2. Token Security

- Refresh tokens are stored within `$XDG_CONFIG_HOME/google-drive-linux/credentials.json` with `0600` file permissions (or the OS Secret Service / GNOME Keyring where available).
- Refresh tokens are revoked via Google's revocation endpoint upon logout.

## 3. Local Cache Safety

- Temporary write buffers are allocated with unique randomized names inside `$XDG_CACHE_HOME/google-drive-linux/staging/` to avoid symlink attacks and race conditions.
- SQLite databases use WAL (Write-Ahead Logging) to ensure atomicity and prevent corruption across sudden power loss or reboots.
