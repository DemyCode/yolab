# Homelab Installer

Single-file Python installer for automated homelab deployment with WiFi support. No dependencies beyond Python stdlib.

## Build ISO

```bash
nix build .#iso
```

The ISO will be in `result/iso/`.

## Boot from ISO

1. Write ISO to USB drive
2. Boot target machine from USB
3. Installer service starts automatically on port 8000
4. Open browser to `http://localhost:8000`
5. If no internet: Configure WiFi via web interface
6. Fill form and click install

## Features

- **Single file**: ~450 lines of Python, no external dependencies
- **Internet required**: Validates connectivity before install
- **WiFi setup**: Web-based WiFi configuration if ethernet not available
- **Auto-detection**: Detects disks via lsblk, RAM for swap sizing
- **Hardware detection**: Uses nixos-generate-config for official hardware detection
- **Git clone**: Clones homelab config from git repository (clean workflow)
- **Web UI**: Clean form with inline HTML
- **Simple**: No FastAPI, no async, just http.server stdlib

## What It Does

1. **Tests internet connectivity**
   - If connected → Show installer form
   - If not connected → Show WiFi setup page

2. **WiFi Setup (if needed)**:
   - Scans available networks
   - Accepts password
   - Connects via NetworkManager
   - Tests connection
   - Redirects to installer form

3. **Main Installation**:
   - Validates internet is still connected
   - Detects available disks (lsblk)
   - Serves web form with detected disks
   - On form submit:
     - **Clones** homelab configuration from git repository (clean git history!)
     - Generates `config.toml` from your input
     - Captures WiFi config (if used) and adds to config.toml
     - Runs `nixos-generate-config --no-filesystems` to detect hardware
     - Runs `disko-install` (partitions disk + installs NixOS in one command)
     - Copies git repository to `/mnt/etc/nixos` (becomes `/etc/nixos` after boot)
   - Shows success message
   - System ready to reboot

**Post-Installation**: The homelab configuration is a git repository at `/etc/nixos`. To apply updates:
```bash
cd /etc/nixos
git pull
nixos-rebuild switch --flake .#homelab
```

WiFi credentials are persisted in `config.toml` and configured via `networking.wireless.networks` in NixOS.

## Technical Details

- **Bootloader**: GRUB with UEFI support (`efiInstallAsRemovable`)
- **Partitions**: ESP (500M) + LVM (swap + root)
- **Swap**: Auto-detected RAM size (max 32GB)
- **UEFI-only**: Requires UEFI boot mode (covers 99% of homelabs)
- **One-command install**: Uses `disko-install` for atomic operation

## User Input

**WiFi Setup (if no ethernet)**:
- Network SSID (dropdown of scanned networks)
- Password (empty for open networks)

**Main Installer Form**:
- Disk to install to (dropdown of detected disks)
- Hostname (defaults to "homelab")
- Timezone (defaults to "UTC")
- Root SSH key (REQUIRED to prevent lockout)
- Git remote URL (REQUIRED - your homelab repository URL)

**Internet Status**: Always displayed at top of installer form (🟢 Connected / 🔴 No Internet)

Everything else configured post-install by editing `/etc/nixos/config.toml` and running `nixos-rebuild`.

## Architecture

```
service.py (~450 lines)
├── test_internet() - Ping 1.1.1.1 to validate connectivity
├── scan_wifi_networks() - Use nmcli to scan networks
├── connect_wifi(ssid, password) - Connect to WiFi via nmcli
├── get_wifi_config() - Extract connected WiFi credentials
├── detect_disks() - Run lsblk, parse JSON
├── detect_ram_size() - Detect RAM, calculate swap (max 32GB)
├── generate_wifi_html() - WiFi setup page with network dropdown
├── generate_html() - Main installer form with internet status
├── generate_config_toml() - Create config.toml with all settings + WiFi
├── run_installation() - Run install steps:
│   ├── git clone from repository (clean git history!)
│   ├── Generate config.toml with WiFi credentials
│   ├── Generate hardware-configuration.nix
│   ├── Run disko-install (partition + install atomically)
│   └── Copy git repo to /mnt/etc/nixos
└── InstallerHandler - Handle routes:
    ├── GET / - Test internet, show WiFi or installer form
    ├── POST /wifi/connect - Connect to WiFi and redirect
    └── POST /install - Validate internet, run installation
```

Zero external dependencies. Just Python stdlib + NetworkManager (nmcli).
