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
nix build .#nixosConfigurations.yolab-installer.config.system.build.isoImage
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

Each app is a standard Helm chart under `apps/catalog/`:

```
apps/catalog/my-app/
  Chart.yaml           # name, version, and the yolab.io/* annotations below
  values.yaml          # defaults
  values.schema.json   # the install form, from `properties.config`
  templates/           # the Kubernetes manifests, including the gateway
```

The YoLab-specific bits are chart annotations, the standard Helm escape hatch:
`yolab.io/display-name`, `yolab.io/icon`, `yolab.io/category`, `yolab.io/uischema`
(which fields are passwords, which is the tunnel subdomain), and `yolab.io/outputs`
(what to surface after install). A chart that declares no `format: tunnel` field
registers no DNS name. No installer code changes are needed for a new app.

---

## Development

```bash
nix develop
pre-commit run --all-files
```

A machine's own `config.toml` lives outside the repo, in `/var/lib/yolab/machine`,
and comes in as the `yolab-machine` flake input (see flake.nix). Never use
`path:.`: it copies the whole working directory into the store (8.6G against
4.8M) every time.

```bash
sudo nixos-rebuild switch --flake .#yolab \
  --override-input yolab-machine path:/var/lib/yolab/machine --no-write-lock-file
```

---

## CI

Every push lints, builds and pushes `wg-sidecar` and `wg-register` to `ghcr.io/demycode/`, and publishes an installer ISO as a GitHub release. Everything is tagged `<branch>-latest`.

---

## Licence

MIT — see [LICENSE](LICENSE).
