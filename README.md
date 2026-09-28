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
  Chart.yaml           # name, version, display name, icon, category
  values.yaml          # defaults
  values.schema.json   # the install form AND what the app page shows
  templates/           # the Kubernetes manifests, including the gateway
```

`Chart.yaml` carries three annotations: `yolab.io/display-name`, `yolab.io/icon` and
`yolab.io/category`. Everything else a developer controls lives in one standard JSON
Schema, `values.schema.json`, with two sections:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["config"],
  "properties": {
    "config": {
      "type": "object",
      "properties": {
        "subdomain":   { "type": "string", "format": "tunnel", "default": "my-app" },
        "password":    { "type": "string", "title": "Admin password", "writeOnly": true, "generate": true },
        "tor_enabled": { "type": "boolean", "title": "Reachable over Tor", "default": false }
      }
    },
    "outputs": {
      "type": "object",
      "readOnly": true,
      "properties": {
        "url":      { "type": "string", "title": "Web URL", "format": "uri",
                      "source": { "logs": "YOLAB_OUTPUT url (\\S+)" } },
        "password": { "type": "string", "title": "Admin password", "format": "secret",
                      "source": { "config": "password" } },
        "onion":    { "type": "string", "title": "Tor address", "format": "uri",
                      "source": { "logs": "YOLAB_OUTPUT onion (\\S+)" },
                      "when": { "properties": { "tor_enabled": { "const": true } } } }
      }
    }
  }
}
```

**`config`** is the install form, rendered in the order written. `format: "tunnel"`
is the app's address (a chart with none registers no DNS name). `writeOnly: true`
marks a credential: masked in the form, never written to non-secret places, kept when
the app is duplicated. Add `generate: true` and YoLab fills it with a random value.
Fields revealed by a toggle (standard `dependencies`/`oneOf`) render under that toggle.

**`outputs`** is what the app page shows once it runs. Each output has a `title`, a
`format` (`text`, `uri`, `secret` or `multiline`), and one `source`:

- `{"logs": "<regex>"}`: the first capture group of a matching line in any container's
  logs, init containers included. YoLab rescans every minute and keeps the latest value
  found, so a value printed once is not lost when the logs roll over.
- `{"config": "<field>"}`: a generated setting, such as the admin password.

`when` is a JSON Schema checked against the app's settings (defaults filled in). When it
does not match, the output is neither shown nor waited for.

`apps/catalog/check_charts.py` enforces all of this. No installer code changes are
needed for a new app.

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
