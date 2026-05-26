#!/usr/bin/env python3
"""
YoLab macOS interactive setup — stdlib only, no external deps.
Writes homelab/ignored/config.toml from user input.
"""

import getpass
import os
import re
import subprocess
import sys
import uuid
from pathlib import Path


# ─── TOML writer (simple, only handles our config shape) ──────────────────────

def _toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, list):
        items = ", ".join(f'"{x}"' if isinstance(x, str) else str(x) for x in v)
        return f"[{items}]"
    # string — escape backslashes and double-quotes
    escaped = str(v).replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def write_toml(data: dict, path: Path) -> None:
    lines = []
    for section, values in data.items():
        lines.append(f"[{section}]")
        for k, v in values.items():
            lines.append(f"{k} = {_toml_value(v)}")
        lines.append("")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines))


# ─── Prompts ──────────────────────────────────────────────────────────────────

def prompt(question: str, default: str = "") -> str:
    suffix = f" [{default}]" if default else ""
    value = input(f"{question}{suffix}: ").strip()
    return value or default


def prompt_bool(question: str, default: bool = True) -> bool:
    suffix = " [Y/n]" if default else " [y/N]"
    value = input(f"{question}{suffix}: ").strip().lower()
    if not value:
        return default
    return value in ("y", "yes")


def generate_wg_keypair() -> tuple[str, str]:
    result = subprocess.run(["wg", "genkey"], capture_output=True, text=True, check=True)
    private_key = result.stdout.strip()
    result = subprocess.run(
        ["wg", "pubkey"], input=private_key, capture_output=True, text=True, check=True
    )
    public_key = result.stdout.strip()
    return private_key, public_key


# ─── Main ─────────────────────────────────────────────────────────────────────

def main():
    if len(sys.argv) < 2:
        print("Usage: setup.py <yolab_dir> [flake_target]")
        sys.exit(1)

    yolab_dir = Path(sys.argv[1])
    flake_target = sys.argv[2] if len(sys.argv) > 2 else "yolab-mac"
    config_path = yolab_dir / "homelab" / "ignored" / "config.toml"

    if config_path.exists():
        overwrite = prompt_bool(f"\nconfig.toml already exists at {config_path}. Overwrite?", default=False)
        if not overwrite:
            print("Keeping existing config.toml.")
            return

    print("\n=== YoLab macOS Setup ===\n")

    hostname = prompt("Hostname", "homelab-mac")
    timezone = prompt("Timezone", "UTC")

    print("\n--- Swarm setup ---")
    wants_swarm = prompt_bool("Enable swarm (connect multiple machines)?")
    swarm: dict = {"enabled": False, "mode": "manager"}
    if wants_swarm:
        is_first = prompt_bool("Is this the first machine (new swarm manager)?")
        swarm = {"enabled": True, "mode": "manager" if is_first else "worker"}

    print("\n--- Tunnel setup ---")
    wants_tunnel = prompt_bool("Register a YoLab tunnel (makes your homelab reachable from the internet)?")

    tunnel: dict = {"enabled": False}

    if wants_tunnel:
        platform_api_url = prompt("YoLab platform API URL", "http://188.245.104.63:5000")
        account_token = prompt("Account token")
        service_name = prompt("Service name", "homelab")

        print("Generating WireGuard key pair...")
        try:
            wg_private_key, wg_public_key = generate_wg_keypair()
        except FileNotFoundError:
            print("WARNING: 'wg' not found. Install wireguard-tools to enable tunnel.")
            wants_tunnel = False
        else:
            print("Registering tunnel with YoLab platform...")
            import json
            import urllib.request

            try:
                auth_headers = {
                    "Content-Type": "application/json",
                    "Authorization": f"Bearer {account_token}",
                }

                # Step 1: create tunnel (WireGuard peer + IPv6, no DNS)
                payload1 = json.dumps({"wg_public_key": wg_public_key}).encode()
                req1 = urllib.request.Request(
                    f"{platform_api_url}/tunnels",
                    data=payload1,
                    headers=auth_headers,
                    method="POST",
                )
                with urllib.request.urlopen(req1, timeout=10) as resp1:
                    tunnel_data = json.loads(resp1.read())

                tunnel_id = tunnel_data["tunnel_id"]
                sub_ipv6 = tunnel_data["sub_ipv6"]

                # Step 2: attach AAAA record for the management domain
                payload2 = json.dumps({
                    "record_type": "AAAA",
                    "name": service_name,
                    "value": sub_ipv6,
                }).encode()
                req2 = urllib.request.Request(
                    f"{platform_api_url}/tunnels/{tunnel_id}/records",
                    data=payload2,
                    headers=auth_headers,
                    method="POST",
                )
                with urllib.request.urlopen(req2, timeout=10) as resp2:
                    record_data = json.loads(resp2.read())

                print(f"Tunnel registered — IPv6: {sub_ipv6}")
                tunnel = {
                    "enabled": True,
                    "platform_api_url": platform_api_url,
                    "account_token": account_token,
                    "tunnel_id": tunnel_id,
                    "wg_private_key": wg_private_key,
                    "wg_public_key": wg_public_key,
                    "sub_ipv6": sub_ipv6,
                    "dns_url": f"https://{record_data['fqdn']}",
                    "wg_server_endpoint": tunnel_data["wg_server_endpoint"],
                    "wg_server_public_key": tunnel_data["wg_server_public_key"],
                }
            except Exception as e:
                print(f"WARNING: Tunnel registration failed: {e}")
                print("Continuing without tunnel. You can configure it later.")

    config = {
        "homelab": {
            "hostname": hostname,
            "timezone": timezone,
            "locale": "en_US.UTF-8",
            "ssh_port": 22,
            "root_ssh_key": "",
            "allowed_ssh_keys": [],
            "homelab_password_hash": "",
            "git_remote": "https://github.com/DemyCode/yolab.git",
        },
        "system": {
            "platform": "darwin",
            "flake_target": flake_target,
            "repo_path": str(yolab_dir),
        },
        "tunnel": tunnel,
        "swarm": swarm,
        "node": {"node_id": str(uuid.uuid4())},
    }

    write_toml(config, config_path)
    print(f"\nConfiguration written to: {config_path}")


if __name__ == "__main__":
    main()
