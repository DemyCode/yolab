# Generic Homelab NixOS Configuration

A generic, declarative NixOS configuration for homelab machines with remote setup capabilities, Docker support, and built-in FRP client for IPv6 tunneling.

## Overview

This directory contains a foundational NixOS configuration designed to work on ANY homelab machine. Unlike specific homelab configurations, this is a generic template that can be remotely configured and customized without hardcoding hardware details or services.

## Key Features

- **Generic by Design**: Works on any x86_64 hardware without hardcoded specifics
- **Remote Configuration**: Setup service allows remote configuration of the homelab
- **Disko Integration**: Declarative disk partitioning with configurable disk device
- **Docker Support**: Built-in Docker and Docker Compose with remote deployment
- **FRP Client**: Automatic IPv6 tunneling via FRP with registration API integration
- **TOML Configuration**: All machine-specific settings in `config.toml`
- **Secure by Default**: SSH key-based auth, Docker enabled, essential packages

## Architecture

This configuration is designed to be deployed on bare metal machines that will:
1. Be initially configured via the setup service
2. Download and run docker-compose files from remote sources
3. Expose services via FRP tunneling for IPv6 connectivity
4. Auto-configure disk partitioning based on detected hardware

## Project Structure

```
homelab/
├── flake.nix                      # Nix flake with disko integration + ISO builder
├── configuration.nix              # Base NixOS configuration
├── disk-config.nix                # Disko disk configuration (generic)
├── config.toml                    # Machine-specific configuration
├── config.toml.example            # Example configuration file
├── installer/                     # USB installer
│   ├── service.py                # Single-file Python installer (237 lines)
│   ├── iso-config.nix            # ISO configuration
│   └── README.md                 # Installer documentation
├── modules/
│   ├── frpc.nix                  # FRP client service module
│   └── homelab-setup.nix         # Remote setup service
└── README.md
```

## Quick Start

### Option A: USB Installer (Recommended)

1. **Build the installer ISO**:
   ```bash
   nix build .#iso
   ```

2. **Write to USB drive**:
   ```bash
   sudo dd if=result/iso/nixos-*.iso of=/dev/sdX bs=4M status=progress
   ```

3. **Boot target machine from USB**

4. **Installer service starts automatically** on `http://0.0.0.0:8000`

5. **Open browser** to `http://localhost:8000` (or from another machine: `http://<machine-ip>:8000`)

6. **Connect to Internet** (REQUIRED):
   - If ethernet is plugged in, installer detects it automatically
   - If WiFi needed, installer shows WiFi setup page:
     - Scan and select network
     - Enter password
     - Test connection

7. **Fill the installer form**:
   - Select disk from dropdown
   - Enter hostname (defaults to "homelab")
   - Enter timezone (defaults to "UTC")
   - Paste SSH key (REQUIRED to prevent lockout)
   - Enter git remote URL (REQUIRED - your homelab git repository)

8. **Click Install**:
   - Configuration is cloned from your git repository
   - Hardware auto-detected via `nixos-generate-config`
   - Disk partitioned with disko
   - System installed
   - WiFi config passed to installed system (if used)
   - Git repository installed to `/etc/nixos`

9. **Reboot** - System is ready!

**Post-Installation Updates**: The configuration is a git repository at `/etc/nixos`. To apply updates:
```bash
cd /etc/nixos
git pull
nixos-rebuild switch --flake .#homelab
```

See [installer/README.md](installer/README.md) for details.

### Option B: Manual Configuration

### 1. Configure Your Homelab

Copy the example configuration and customize it:

```bash
cp config.toml.example config.toml
```

Edit `config.toml` with your settings:

```toml
[homelab]
hostname = "my-homelab"
timezone = "America/New_York"
ssh_port = 22
root_ssh_key = "ssh-ed25519 AAAAC3... root@example.com"
allowed_ssh_keys = [
    "ssh-ed25519 AAAAC3... user@example.com"
]

[disk]
device = "/dev/sda"  # Detected during setup
boot_size = "1M"
esp_size = "500M"

[setup]
enabled = true  # Enable remote configuration service
port = 5001
registration_api_url = "http://your-backend:5000"

[docker]
enabled = true
compose_url = "https://example.com/docker-compose.yml"

[frpc]
enabled = true
server_addr = "2001:db8::1"
server_port = 7000
account_token = "your-account-token-from-registration-api"

[[frpc.services]]
name = "ssh"
type = "tcp"
local_port = 22
remote_port = 10022
```

### 2. Get Your Account Token

Get an account token from the registration API:

```bash
curl -X POST http://your-registration-api:5000/api/token/new
```

This will return an `account_token` that you'll use in `config.toml`.

### 3. Configure Your Services

Add services to expose via FRP in `config.toml`:

```toml
[[frpc.services]]
name = "ssh"
type = "tcp"
local_port = 22
remote_port = 10022
description = "SSH access"

[[frpc.services]]
name = "web"
type = "tcp"
local_port = 80
remote_port = 10080
description = "Web server"
```

### 4. Build and Deploy

Build the NixOS configuration:

```bash
nixos-rebuild switch --flake .#homelab
```

Or test without switching:

```bash
nixos-rebuild test --flake .#homelab
```

## Deployment Workflow

### Initial Deployment

1. **Prepare configuration**: Customize `config.toml` with basic settings (hostname, SSH keys, disk device)
2. **Deploy with nixos-anywhere**:
   ```bash
   nixos-anywhere --flake .#homelab root@target-machine
   ```
3. **System boots**: Machine comes up with SSH access and setup service running
4. **Remote configuration**: Use the setup service API to configure additional settings
5. **Docker deployment**: System downloads and deploys docker-compose.yml automatically
6. **FRP registration**: Services register with the registration API and start tunneling

### Remote Configuration Service

The homelab-setup service provides an HTTP API for remote configuration:

```bash
# Check health
curl http://homelab-ip:5001/health

# Check status
curl http://homelab-ip:5001/status

# Configure system (placeholder for future implementation)
curl -X POST http://homelab-ip:5001/configure \
  -H "Content-Type: application/json" \
  -d '{"disk": "/dev/nvme0n1", "docker_compose_url": "https://..."}'
```

## Configuration Reference

### `[homelab]` Section

- `hostname`: Machine hostname (default: "homelab")
- `timezone`: System timezone (default: "UTC")
- `locale`: System locale (default: "en_US.UTF-8")
- `ssh_port`: SSH port (default: 22)
- `root_ssh_key`: Root SSH public key for initial access (optional)
- `allowed_ssh_keys`: List of SSH public keys for the homelab user

### `[disk]` Section

- `device`: Primary disk device (default: "/dev/sda")
- `esp_size`: EFI system partition size (default: "500M")
- `swap_size`: Swap partition size (auto-detected during install, max 32GB)

**Note**: This configuration is UEFI-only with GRUB bootloader using `efiInstallAsRemovable` for maximum compatibility. Swap size is automatically set to match RAM size (capped at 32GB) during installation.

### `[setup]` Section

- `enabled`: Enable homelab setup service (default: false)
- `port`: HTTP port for setup service (default: 5001)
- `registration_api_url`: URL of the registration API

### `[docker]` Section

- `enabled`: Enable Docker Compose deployment (default: false)
- `compose_url`: URL to download docker-compose.yml from (optional)

### `[frpc]` Section

- `enabled`: Enable FRP client services (default: false)
- `account_token`: Your account token from the registration API
- `server_addr`: FRP server IPv6 address (loaded from registration)
- `server_port`: FRP server port (default: 7000)

### `[[frpc.services]]` Array

Each service defines a tunnel:

- `name`: Service name (must match registration API pattern)
- `type`: Protocol type ("tcp" or "udp")
- `local_port`: Local port to tunnel
- `remote_port`: Remote port to expose on
- `description`: Optional description

## How It Works

1. **Build Time**: `configuration.nix` loads settings from `config.toml`
2. **Disk Setup**: `disk-config.nix` creates partitions using disko based on config
   - ESP partition (500M) for UEFI boot
   - LVM physical volume with swap (RAM size, max 32GB) + root (remaining space)
3. **Boot**: System starts with Docker, SSH, and setup service
4. **Docker Deploy**: If enabled, downloads and runs docker-compose.yml
5. **FRP Services**: `frpc.nix` generates systemd services for each tunnel
6. **Remote Config**: Setup service allows post-deployment configuration

## Differences from Specific Homelab Config

This generic configuration differs from a personal homelab config:

| Feature | Generic Homelab | Personal Homelab |
|---------|----------------|------------------|
| Hardware | Configurable via TOML | Hardcoded for specific machine |
| Docker Compose | Downloaded from URL | Bundled in repository |
| Disk Device | Configurable | Fixed (e.g., /dev/nvme0n1) |
| Services | Generic, minimal | Specific (Jellyfin, Immich, etc.) |
| Backups | Not configured | Restic to B2 |
| Secrets | In config.toml | In secrets.toml |
| Setup | Remote configuration API | Pre-configured |

This makes it suitable for deploying to multiple machines with different hardware and service requirements.

## Service Management

Check FRP service status:

```bash
systemctl status frpc-ssh
systemctl status frpc-web
```

View logs:

```bash
journalctl -u frpc-ssh -f
```

Restart a service:

```bash
systemctl restart frpc-ssh
```

## Security Considerations

- SSH is configured with key-based authentication only (no passwords)
- The `homelab` user is created with sudo access (passwordless)
- FRP services run as a dedicated `frpc` system user
- Firewall is enabled by default (only SSH port is open)
- Root login is disabled

## Customization

### Adding More Packages

Edit `configuration.nix` and add packages to `environment.systemPackages`:

```nix
environment.systemPackages = with pkgs; [
  vim
  wget
  curl
  # Add your packages here
  docker
  nginx
];
```

### Hardware Configuration

For real hardware deployments, you'll need to add a `hardware-configuration.nix`:

```bash
nixos-generate-config --show-hardware-config > hardware-configuration.nix
```

Then import it in `configuration.nix`:

```nix
imports = [
  ./hardware-configuration.nix
];
```

## Troubleshooting

### FRP Client Not Connecting

1. Check if the service is running: `systemctl status frpc-<service-name>`
2. View logs: `journalctl -u frpc-<service-name> -f`
3. Verify your `account_token` is correct
4. Ensure IPv6 connectivity is working
5. Check if the registration API registered your service

### SSH Access Issues

1. Verify your public key is in `config.toml`
2. Check if SSH service is running: `systemctl status sshd`
3. Verify the SSH port in your firewall: `sudo firewall-cmd --list-ports`

## Coming Soon

- Docker Compose integration
- Kubernetes configurations
- Self-hosted services (Jellyfin, Nextcloud, etc.)
- Monitoring and observability (Prometheus, Grafana)
- Automated backup solutions
