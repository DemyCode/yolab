# YoLab Homelab Installer

Modern web-based installer for automated homelab deployment with WiFi support. Features a FastAPI backend and React TypeScript frontend.

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

- **Modern UI**: React + TypeScript frontend with clean, dark terminal aesthetic
- **FastAPI Backend**: RESTful API with proper async support
- **Internet required**: Validates connectivity before install
- **WiFi setup**: Interactive network scanning and connection
- **Auto-detection**: Detects disks via lsblk, RAM for swap sizing
- **Hardware detection**: Uses nixos-generate-config for official hardware detection
- **Git clone**: Clones homelab config from git repository (clean workflow)
- **Type-safe**: Full TypeScript support for maintainability

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
nixos-rebuild switch --flake .#yolab
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
installer/
├── backend/
│   ├── main.py - FastAPI backend with async endpoints
│   │   ├── GET /api/status - Get internet status and disk list
│   │   ├── GET /api/wifi/scan - Scan available WiFi networks
│   │   ├── POST /api/wifi/connect - Connect to WiFi
│   │   └── POST /api/install - Run installation process
│   └── pyproject.toml - Python dependencies (fastapi, uvicorn, pydantic)
├── frontend/
│   ├── src/
│   │   ├── App.tsx - Main application component
│   │   ├── components/
│   │   │   ├── WifiSetup.tsx - WiFi configuration interface
│   │   │   └── InstallForm.tsx - Installation form with disk selection
│   │   └── index.css - Terminal-style dark theme
│   ├── package.json - Node dependencies (react, typescript, vite)
│   └── tsconfig.json - TypeScript configuration
└── iso-config.nix - NixOS ISO configuration
    ├── Builds frontend with buildNpmPackage
    ├── Creates Python environment with FastAPI
    └── Configures systemd service to run installer
```

**Backend Functions**:
- `test_internet()` - Ping 1.1.1.1 to validate connectivity
- `scan_wifi_networks()` - Use nmcli to scan networks
- `connect_wifi()` - Connect to WiFi via nmcli
- `get_wifi_config()` - Extract connected WiFi credentials
- `detect_disks()` - Run lsblk, parse JSON
- `detect_ram_size()` - Detect RAM, calculate swap (max 32GB)
- `generate_config_toml()` - Create config.toml with all settings
- `run_installation()` - Run complete installation process

**Frontend Components**:
- `App.tsx` - Main container, manages state and API calls
- `WifiSetup.tsx` - WiFi network scanning and connection
- `InstallForm.tsx` - Disk selection and system configuration
