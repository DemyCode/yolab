# YoLab

Self-hosted apps on hardware you own, without the networking headaches.

Boot the installer on any machine — bare metal, a VPS, an old laptop. It provisions NixOS, connects to a WireGuard hub, and joins a K3s cluster. Install apps from a web UI: each one gets its own tunnel endpoint, subdomain, and TLS certificate. No port-forwarding, no DDNS, no reverse proxy config to maintain every time you add something new.

Add more machines with the same account token and they join the cluster automatically. Three nodes and the control plane is HA.

---

## Apps

| | App | Description |
|---|---|---|
| 🐙 | **Gitea** | Self-hosted Git |
| 📸 | **Immich** | Photo and video backup |
| 🔐 | **Vaultwarden** | Bitwarden-compatible password manager |
| 🔑 | **2FAuth** | Two-factor authentication manager |
| 🔔 | **ntfy** | Push notifications to any device |
| ⚡ | **LibreSpeed** | Self-hosted speed test |
| 💬 | **Cinny** | Matrix client |
| 📡 | **Strfry** | Nostr relay |
| ⛏️ | **Minecraft** | Java Edition server |

---

## Getting started

### Prerequisites

- A YoLab platform account — provides the WireGuard hub and DNS
- A machine to install on (bare-metal, VM, or VPS with KVM)

### 1. Get the ISO

Download from [Releases](../../releases), or build it:

```bash
nix build path:.#iso
```

### 2. Boot the installer

```bash
sudo dd if=result/iso/*.iso of=/dev/sdX bs=4M status=progress && sync
```

Boot from USB. The installer UI starts on `tty1` and is also reachable as a web app on port 80.

### 3. Install

Enter your account token, configure the disk, and let it run. When the machine reboots, the management UI is live at your node's tunnel address.

### 4. Install apps

Go to **Apps**, pick something from the catalog, fill in the form, click Install.

---

## Adding an app to the catalog

Each app is a directory under `apps/catalog/` with six files:

```
apps/catalog/my-app/
  app.toml           # name, icon, category
  schema.json        # config fields shown in the install form
  uischema.json      # field labels, order, widget hints
  manifest.yaml.j2   # Kubernetes manifest template (Jinja2)
  outputs.json       # values to surface after install (URL, credentials…)
  uninstall.yaml.j2  # cleanup job — tears down the tunnel and namespace
```

The installer renders the manifest and applies it. No installer code changes needed for new apps.

---

## Development

```bash
nix develop
pre-commit run --all-files
```

WSL and macOS configs are included:

```bash
sudo nixos-rebuild switch --flake path:.#yolab-wsl
darwin-rebuild switch --flake path:.#yolab-mac
```

---

## CI

Every push lints, builds and pushes `wg-sidecar` and `wg-register` to `ghcr.io/demycode/`, and publishes an installer ISO as a GitHub release. Everything is tagged `<branch>-latest`.
