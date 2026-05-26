# YoLab

A self-hosted homelab platform that turns one or more bare-metal machines into a managed Kubernetes cluster — with a web UI for installing apps, automated WireGuard tunneling, and NixOS-based reproducible configuration.

## What it does

You boot the YoLab installer ISO on any machine. The installer provisions NixOS, joins the machine to a K3s cluster, and connects it to your WireGuard hub. From there, a web UI lets you install self-hosted apps with one click. Each app gets its own encrypted tunnel address, TLS certificate, and persistent storage — no port-forwarding, no Cloudflare account, no reverse proxy to maintain manually.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  Your device (browser)                              │
│      ↓  HTTPS                                       │
│  Caddy (management UI)  ←──  local-api (FastAPI)   │
│      ↑  WireGuard tunnel (wg0)                      │
├─────────────────────────────────────────────────────┤
│  YoLab node (NixOS + K3s)                           │
│                                                     │
│  ┌──────────────────────────────────────────────┐   │
│  │  App pod (per installed app)                 │   │
│  │   wg-register (init) → writes /yolab/env     │   │
│  │   wireguard sidecar  → own tunnel endpoint   │   │
│  │   caddy              → TLS termination       │   │
│  │   app container      → the actual service    │   │
│  └──────────────────────────────────────────────┘   │
│                                                     │
│  NFS server  →  PersistentVolumes for app data      │
└─────────────────────────────────────────────────────┘
```

**Key design decisions:**

- **WireGuard-first networking** — each app gets its own tunnel endpoint registered with a central hub. No shared ingress, no LoadBalancer service. Apps are isolated at the network level.
- **All-in-one pod pattern** — app container, WireGuard sidecar, and Caddy live in one Deployment. The app sources `/yolab/env` (written by the `wg-register` init container) to get its public FQDN at startup.
- **NFS for storage** — each node exports its own disks. App PVCs mount over NFS so workloads can be rescheduled to any node without moving data.
- **Traefik disabled** — K3s ships with Traefik but it conflicts with Caddy. YoLab disables it at the cluster level.
- **IPv6-native** — inter-node traffic and all app tunnels use IPv6. VXLAN Flannel backend runs on top of the existing WireGuard mesh (single encryption layer, no double-encapsulation).

## Repository layout

```
apps/
  catalog/          # App catalog — one directory per installable app
    gitea/
    immich/
    vaultwarden/
    librespeed/
    minecraft/
    ntfy/
    2fauth/
    cinny/
    strfry/
    ...
  wg-register/      # Init container: registers tunnel, writes /yolab/env
  wg-sidecar/       # Sidecar: maintains per-app WireGuard tunnel

homelab/
  nixos/            # NixOS configuration (bare-metal + QEMU)
  darwin/           # nix-darwin configuration (macOS dev machines)
  local-api/        # FastAPI backend — runs on every node
  client-ui/        # React frontend — served by Caddy

installer/
  nixos/            # ISO configuration (NixOS live installer)
  frontend/         # Installer web UI
  backend/          # Installer Python backend

flake.nix           # Single flake: NixOS systems, ISO, dev shell
```

## App catalog

Each app is a directory under `apps/catalog/` with six files:

| File | Purpose |
|---|---|
| `app.toml` | App metadata (id, name, icon, category) |
| `schema.json` | JSON Schema for user-facing config fields |
| `uischema.json` | UI hints (field order, labels, widgets) |
| `manifest.yaml.j2` | Jinja2 Kubernetes manifest template |
| `outputs.json` | Outputs to scan from pod logs after install |
| `uninstall.yaml.j2` | Cleanup job (deregisters WireGuard tunnel, deletes namespace) |

The installer backend (`local-api`) renders the manifest template and applies it with `kubectl`. No app-specific logic lives in the installer — all app knowledge stays in the catalog.

### Available apps

| App | Category | Description |
|---|---|---|
| Gitea | Development | Self-hosted Git service |
| Immich | Media | Photo and video backup |
| Vaultwarden | Productivity | Bitwarden-compatible password manager |
| LibreSpeed | Utilities | Self-hosted speed test |
| Minecraft | Gaming | Java Edition server |
| ntfy | Utilities | Push notification service |
| 2FAuth | Security | Two-factor authentication manager |
| Cinny | Communication | Matrix web client |
| Strfry | Communication | Nostr relay |

## Getting started

### Prerequisites

- A [YoLab platform](https://github.com/demycode/yolab-external) account (provides the WireGuard hub and DNS)
- A machine to install on (bare-metal, VM, or a VPS with KVM)
- A USB drive (for bare-metal installs)

### 1. Build or download the ISO

Download the latest ISO from the [Releases](../../releases) page, or build it locally:

```bash
nix build path:.#iso
```

### 2. Boot the installer

Write the ISO to a USB drive:

```bash
sudo dd if=result/iso/*.iso of=/dev/sdX bs=4M status=progress && sync
```

Boot the machine from USB. The installer UI starts automatically on `tty1` and is also accessible as a web app on port 80.

### 3. Configure and install

The installer walks you through:
1. Connecting to your WireGuard hub (enter your account token)
2. Configuring the disk layout
3. Running the NixOS installation

Once the machine reboots, the management UI is available at your node's tunnel address.

### 4. Install apps

Open the management UI, go to **Apps**, pick an app from the catalog, fill in the config form, and click **Install**. The app is deployed to K3s and available at its own subdomain within a minute or two.

## Development

Enter the dev shell:

```bash
nix develop
```

This provides: `alejandra`, `statix`, `deadnix`, `shellcheck`, `hadolint`, `uv`, `nodejs`, `pre-commit`.

Run pre-commit checks:

```bash
pre-commit run --all-files
```

### Multi-node clusters

Add nodes by running the installer on additional machines with the same account token. Nodes discover each other through the WireGuard hub. K3s embedded etcd provides HA once there are 3+ nodes.

### WSL / macOS

YoLab includes configurations for development on WSL and macOS:

```bash
# WSL
sudo nixos-rebuild switch --flake path:.#yolab-wsl

# macOS
darwin-rebuild switch --flake path:.#yolab-mac
```

## CI

Every push runs three jobs:

- **pre-commit** — linting and formatting (ruff, ESLint, alejandra, hadolint, shellcheck)
- **build-images** — builds and pushes `wg-sidecar` and `wg-register` to `ghcr.io/demycode/`
- **build-iso** — builds the NixOS installer ISO and creates a GitHub release

Images and releases are tagged `<branch>-latest` (e.g. `main-latest`, `feature-my-thing-latest`).
