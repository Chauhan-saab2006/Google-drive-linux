# Google Drive Linux — Troubleshooting Guide

## 1. Common Issues & Solutions

### Issue: "fusermount: user has no write access to mountpoint"
**Cause**: The target mount directory (e.g. `~/GoogleDrive`) does not exist or has incorrect ownership.
**Fix**:
```bash
mkdir -p ~/GoogleDrive
chmod 755 ~/GoogleDrive
```

### Issue: "Device or resource busy" during unmount
**Cause**: A terminal or application currently has an open file descriptor or working directory inside `~/GoogleDrive`.
**Fix**:
1. Check what process is holding the directory:
   ```bash
   lsof +D ~/GoogleDrive
   ```
2. Close the offending application or force unmount via fusermount3:
   ```bash
   fusermount3 -u ~/GoogleDrive
   # Or lazy unmount if unresponsive:
   fusermount3 -z -u ~/GoogleDrive
   ```

### Issue: "Authentication required" or "Token expired"
**Cause**: The user revoked Google Drive OAuth access or the token expired without network connectivity to refresh.
**Fix**:
Run `google-drive-linux login` or use the GUI button to re-authorize.

### Issue: Background service not starting on boot
**Cause**: Systemd user service is disabled or failed.
**Fix**:
```bash
systemctl --user status google-drive-linux.service
journalctl --user -u google-drive-linux.service -e
```
