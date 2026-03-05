import os
import subprocess
import time

import httpx

BACKEND_URL = os.environ["BACKEND_URL"]
POLL_INTERVAL = int(os.environ.get("POLL_INTERVAL", "30"))
WG_INTERFACE = os.environ.get("WG_INTERFACE", "wg0")


def get_current_peers() -> dict[str, str]:
    """Return dict of pubkey -> first ipv6 for all peers currently in WireGuard."""
    result = subprocess.run(
        ["wg", "show", WG_INTERFACE, "allowed-ips"],
        capture_output=True,
        text=True,
    )
    peers: dict[str, str] = {}
    if result.returncode != 0:
        return peers
    for line in result.stdout.strip().splitlines():
        parts = line.split()
        if len(parts) >= 2:
            pubkey = parts[0]
            ipv6 = parts[1].split("/")[0]
            peers[pubkey] = ipv6
    return peers


def sync_peers() -> None:
    try:
        resp = httpx.get(f"http://{BACKEND_URL}/wireguard/peers", timeout=10)
        resp.raise_for_status()
    except Exception as e:
        print(f"Failed to fetch peers from backend: {e}", flush=True)
        return

    # Group IPs by pubkey — wg set replaces allowed-ips so all IPs must be set at once
    desired: dict[str, list[str]] = {}
    for p in resp.json():
        desired.setdefault(p["wg_public_key"], []).append(p["sub_ipv6"])

    current = get_current_peers()

    for pubkey, ipv6 in current.items():
        subprocess.run(
            ["ip", "-6", "route", "add", f"{ipv6}/128", "dev", WG_INTERFACE],
            capture_output=True,
        )

    for pubkey, ipv6s in desired.items():
        allowed = ",".join(f"{ip}/128" for ip in ipv6s)
        if pubkey not in current:
            print(f"Adding peer {pubkey[:8]}... -> {allowed}", flush=True)
            subprocess.run(
                ["wg", "set", WG_INTERFACE, "peer", pubkey, "allowed-ips", allowed],
                check=True,
            )
        else:
            # Update allowed-ips in case new IPs were added for an existing peer
            subprocess.run(
                ["wg", "set", WG_INTERFACE, "peer", pubkey, "allowed-ips", allowed],
                check=True,
            )
        for ipv6 in ipv6s:
            subprocess.run(
                ["ip", "-6", "route", "add", f"{ipv6}/128", "dev", WG_INTERFACE],
                capture_output=True,
            )

    for pubkey, ipv6 in current.items():
        if pubkey not in desired:
            print(f"Removing peer {pubkey[:8]}...", flush=True)
            subprocess.run(
                ["ip", "-6", "route", "del", f"{ipv6}/128", "dev", WG_INTERFACE],
                capture_output=True,
            )
            subprocess.run(
                ["wg", "set", WG_INTERFACE, "peer", pubkey, "remove"],
                check=True,
            )


def main() -> None:
    print(f"WireGuard manager started. Backend: {BACKEND_URL}, interval: {POLL_INTERVAL}s", flush=True)
    while True:
        sync_peers()
        time.sleep(POLL_INTERVAL)
