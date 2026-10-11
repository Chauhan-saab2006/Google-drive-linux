# Google Drive Linux — Installation Guide

## 1. Distribution Support

Target distributions:
- **Ubuntu 24.04 LTS / 26.04 LTS**
- **Debian 12 / 13**
- **Arch Linux / CachyOS / Manjaro**
- **Fedora 40+**

Target architecture:
- `x86_64` (Initial release)
- `aarch64` (Planned)

---

## 2. Installing Dependencies

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

## 3. Building and Installing from Source

```bash
git clone https://github.com/your-org/google-drive-linux.git
cd google-drive-linux
cargo build --release
sudo cp target/release/google-drive-linux /usr/local/bin/
```

### Install Desktop Integration & Systemd Service
```bash
# Desktop launcher
mkdir -p ~/.local/share/applications
cp packaging/google-drive-linux.desktop ~/.local/share/applications/

# Systemd user service
mkdir -p ~/.config/systemd/user
cp packaging/google-drive-linux.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now google-drive-linux.service
```
