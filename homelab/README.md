# YoLab Homelab

NixOS-based homelab configuration for YoLab IPv6 tunneling platform.

## Directory Structure

```
homelab/
├── ignored/
│   ├── config.toml              # Your configuration (gitignored)
│   ├── config.toml.example      # Configuration template
│   └── hardware-configuration.nix  # Hardware config (gitignored)
├── nixos/
│   ├── configuration.nix        # Main system configuration
│   ├── disk-config.nix          # Disk partitioning (Disko)
│   └── modules/
│       ├── frpc.nix             # FRP client service
│       └── client-ui.nix        # Client UI service
├── scripts/
│   ├── validate-config.sh       # Validate configuration
│   └── safe-update.sh           # Safe system update
├── installer/                   # ISO installer
├── client-ui/                   # Web UI for management
├── flake.nix                    # Nix flake definition
└── README.md                    # This file
```

## Quick Start

### 1. Clone the Repository

```bash
# Clone to /opt/yolab (recommended location)
sudo git clone https://github.com/yourusername/yolab.git /opt/yolab
cd /opt/yolab/homelab
```

### 2. Create Configuration

```bash
# Copy example config
cp ignored/config.toml.example ignored/config.toml

# Edit with your settings
vim ignored/config.toml
```

**Example Configuration:**
```toml
[homelab]
hostname = "yolab-client"
timezone = "America/New_York"
locale = "en_US.UTF-8"
ssh_port = 22
root_ssh_key = "ssh-ed25519 AAAA... root@host"
allowed_ssh_keys = [
    "ssh-ed25519 AAAA... user@host"
]

[client_ui]
enabled = true
port = 8080
platform_api_url = "https://your-backend-server.com"

[frpc]
enabled = true
server_addr = "your-frp-server.com"
server_port = 7000
account_token = "your-account-token-here"

[[frpc.services]]
name = "web"
type = "tcp"
local_port = 8080
remote_port = 10080
description = "Web service"
```

### 3. Validate Configuration

```bash
# Run validation script
./scripts/validate-config.sh
```

### 4. Build and Deploy

```bash
# Initial deployment (from the machine being configured)
sudo nixos-rebuild switch --flake .#yolab

# Or use the safe update script
sudo ./scripts/safe-update.sh
```

## Updating the System

Use the **safe update script** to update your system:

```bash
# Normal update (validates, fetches, and rebuilds)
sudo ./scripts/safe-update.sh

# Dry run (show what would happen)
sudo ./scripts/safe-update.sh --dry-run

# Test build without switching
sudo ./scripts/safe-update.sh --test-only

# Rollback to previous generation
sudo ./scripts/safe-update.sh --rollback
```

### Update Workflow

The safe update script:

1. ✅ **Validates** your configuration
2. 🔍 **Fetches** updates from remote
3. 📊 **Shows** what changed (commits)
4. ❓ **Asks** for confirmation
5. 💾 **Backs up** current config
6. ⬇️ **Pulls** updates
7. ✅ **Re-validates** configuration
8. 🔨 **Builds** and switches to new system

### Manual Update (Alternative)

```bash
cd /opt/yolab/homelab

# Validate config
./scripts/validate-config.sh

# Fetch and pull updates
git fetch origin main
git pull origin main

# Rebuild system
sudo nixos-rebuild switch --flake .#yolab
```

## Configuration Reference

### `[homelab]` Section

**Required fields:**
- `hostname`: Machine hostname
- `timezone`: System timezone (e.g., "America/New_York", "UTC")
- `locale`: System locale (default: "en_US.UTF-8")
- `ssh_port`: SSH port (default: 22)
- `allowed_ssh_keys`: List of SSH public keys for the homelab user

**Optional:**
- `root_ssh_key`: Root SSH public key (for root access)

### `[disk]` Section

- `device`: Primary disk device (default: "/dev/sda")
- `esp_size`: EFI system partition size (default: "500M")
- `swap_size`: Swap partition size (default: "8G")

### `[client_ui]` Section

- `enabled`: Enable client UI web interface (default: true)
- `port`: HTTP port for client UI (default: 8080)
- `platform_api_url`: URL of your YoLab backend API server

### `[docker]` Section

- `enabled`: Enable Docker Compose deployment (default: false)
- `compose_url`: URL to download docker-compose.yml from (optional)

### `[frpc]` Section

- `enabled`: Enable FRP client services (default: false)
- `server_addr`: FRP server address
- `server_port`: FRP server port (default: 7000)
- `account_token`: Your YoLab account token

### `[wifi]` Section

- `enabled`: Enable WiFi setup at boot (default: false)
- `ssid`: WiFi network name
- `psk`: WiFi password

### `[[frpc.services]]` Array

Each service defines a tunnel:

- `name`: Service name
- `type`: Protocol type ("tcp" or "udp")
- `local_port`: Local port to tunnel
- `remote_port`: Remote port to expose on
- `description`: Optional description

## Configuration Management

### Editing Configuration

```bash
# Edit config
vim /opt/yolab/homelab/ignored/config.toml

# Validate changes
./scripts/validate-config.sh

# Apply changes
sudo nixos-rebuild switch --flake /opt/yolab/homelab#yolab
```

### Adding FRP Services

Edit `ignored/config.toml`:

```toml
[[frpc.services]]
name = "ssh"
type = "tcp"
local_port = 22
remote_port = 10022
description = "SSH access"
```

Then rebuild:
```bash
sudo nixos-rebuild switch --flake /opt/yolab/homelab#yolab
```

## Troubleshooting

### Configuration Validation Fails

```bash
# Check TOML syntax
python3 -c "import tomllib; tomllib.load(open('ignored/config.toml', 'rb'))"

# Run validation script
./scripts/validate-config.sh
```

### Build Fails

```bash
# Check build errors with trace
sudo nixos-rebuild switch --flake .#yolab --show-trace

# Rollback to previous generation
sudo ./scripts/safe-update.sh --rollback
```

### Git Pull Fails (Dirty Working Tree)

```bash
# Check what's modified
git status

# ignored/ directory is gitignored, so this shouldn't happen
# If other files are modified, stash them:
git stash

# Then pull
git pull origin main
```

### Service Not Starting

```bash
# Check FRP service status
sudo systemctl status frpc-servicename

# Check client UI status
sudo systemctl status yolab-client-ui

# View logs
sudo journalctl -u frpc-servicename -f
sudo journalctl -u yolab-client-ui -f
```

## Client UI

The Client UI runs on the configured port (default: 8080) and provides:

- Configuration management
- Service template download
- System rebuild interface
- Service monitoring

Access at: `http://your-hostname:8080`

## Service Management

### FRP Services

```bash
# Check all FRP services
sudo systemctl list-units 'frpc-*'

# Check specific service
sudo systemctl status frpc-web

# View logs
sudo journalctl -u frpc-web -f

# Restart service
sudo systemctl restart frpc-web
```

### Client UI

```bash
# Check status
sudo systemctl status yolab-client-ui

# View logs
sudo journalctl -u yolab-client-ui -f

# Restart
sudo systemctl restart yolab-client-ui
```

### Docker Services

```bash
# Check Docker Compose service
sudo systemctl status homelab-docker-compose

# View logs
sudo journalctl -u homelab-docker-compose -f

# Restart
sudo systemctl restart homelab-docker-compose
```

## Development

### Testing Changes Locally

```bash
# Edit configuration files
vim nixos/configuration.nix

# Validate
./scripts/validate-config.sh

# Test build (doesn't switch)
sudo nixos-rebuild build --flake .#yolab

# Test without making it default
sudo nixos-rebuild test --flake .#yolab

# If satisfied, switch
sudo nixos-rebuild switch --flake .#yolab
```

### Contributing

When contributing changes:

1. **Never commit user config**: Ensure `.gitignore` excludes `ignored/config.toml` and `ignored/hardware-configuration.nix`
2. **Update config.toml.example**: If adding new config options
3. **Update documentation**: Keep this README in sync
4. **Test changes**: Validate config and test build before committing

## USB Installer

For bare metal installations, use the USB installer:

```bash
# Build installer ISO
nix build .#iso

# Write to USB
sudo dd if=result/iso/nixos-*.iso of=/dev/sdX bs=4M status=progress
```

See `installer/README.md` for detailed installation instructions.

## Architecture Notes

### Why `ignored/` Directory?

- **Clean separation**: User-specific config separate from code
- **Git-friendly**: Entire `ignored/` directory is gitignored (except example files)
- **Simple**: Everything in one place - config.toml contains all settings including secrets
- **Relative paths**: Works perfectly with Nix flakes

### Git Workflow

```bash
# Your workflow
cd /opt/yolab/homelab
git pull                  # Updates code only (ignored/ is gitignored)
sudo nixos-rebuild switch --flake .#yolab  # Applies your config
```

The `ignored/` directory stays local to your machine and never gets committed.

## Support

For issues or questions:
- GitHub Issues: https://github.com/yourusername/yolab/issues
- Documentation: See inline comments in configuration files

## License

[Your License Here]
