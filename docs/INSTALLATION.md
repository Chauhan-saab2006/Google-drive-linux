# Google Drive Linux — Installation Guide

## 1. Distribution Support

Target distributions:
- **Arch Linux / CachyOS / Manjaro**
- **Ubuntu 24.04 LTS / 26.04 LTS**
- **Debian 12 / 13 / Linux Mint / Pop!_OS**
- **Fedora 40+**

Target architecture:
- `x86_64` (Initial release)
- `aarch64` (Planned)

---

## 2. Installing Pre-built Packages (Recommended)

Packages for various Linux environments are built into the `dist/` folder:

### Option A: Universal Standalone Bundle (Any Linux Distribution)
No root privileges required.
```bash
tar -xzf dist/google-drive-linux-0.1.0-linux-x86_64.tar.gz
cd google-drive-linux-0.1.0-x86_64
./install.sh
```
This installs the binary into `~/.local/bin/`, the application icon, and registers the `.desktop` launcher so it immediately appears in your GNOME / KDE / XFCE app menu.

### Option B: Arch Linux / CachyOS / Manjaro
```bash
sudo pacman -U dist/google-drive-linux-bin-0.1.0-1-x86_64.pkg.tar.zst
```

### Option C: Ubuntu / Debian / Linux Mint / Pop!_OS
```bash
sudo apt install ./dist/google-drive-linux_0.1.0_amd64.deb
# Or:
# sudo dpkg -i dist/google-drive-linux_0.1.0_amd64.deb
```

---

## 3. Installing Dependencies

### Arch Linux / CachyOS
```bash
sudo pacman -S fuse3 gtk4 libadwaita sqlite openssl
```

### Ubuntu / Debian
```bash
sudo apt update
sudo apt install fuse3 libgtk-4-1 libadwaita-1-0 libsqlite3-0 libssl3
```

---

## 4. Building and Packaging from Source

To compile the latest release and regenerate all package formats:
```bash
./scripts/package.sh
```
This produces:
- `dist/google-drive-linux-0.1.0-linux-x86_64.tar.gz`
- `dist/google-drive-linux-bin-0.1.0-1-x86_64.pkg.tar.zst`
- `dist/google-drive-linux_0.1.0_amd64.deb`

To install desktop integration directly on the current system:
```bash
./scripts/install-desktop.sh
```
