import os
import subprocess
import time

import httpx

BACKEND_URL = os.environ["BACKEND_URL"]
POLL_INTERVAL = int(os.environ.get("POLL_INTERVAL", "30"))
WG_INTERFACE = os.environ.get("WG_INTERFACE", "wg0")


def get_current_peers() -> dict[str, str]:
    """Return dict of pubkey -> ipv6 for all peers currently in WireGuard."""
    result = subprocess.run(
        ["wg", "show", WG_INTERFACE, "allowed-ips"],
        capture_output=True,
        text=True,
    )
    peers: dict[str, str] = {}
    for line in result.stdout.strip().splitlines():
        parts = line.split()
        if len(parts) >= 2:
            pubkey = parts[0]
            # allowed-ips may be "fd00::13/128" — take the first entry
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

    desired: dict[str, str] = {p["wg_public_key"]: p["sub_ipv6"] for p in resp.json()}
    current = get_current_peers()

    for pubkey, ipv6 in current.items():
        subprocess.run(
            ["ip", "-6", "route", "add", f"{ipv6}/128", "dev", WG_INTERFACE],
            capture_output=True,
        )

    for pubkey, ipv6 in desired.items():
        if pubkey not in current:
            print(f"Adding peer {pubkey[:8]}... -> {ipv6}/128", flush=True)
            subprocess.run(
                ["wg", "set", WG_INTERFACE, "peer", pubkey, "allowed-ips", f"{ipv6}/128"],
                check=True,
            )
            subprocess.run(
                ["ip", "-6", "route", "add", f"{ipv6}/128", "dev", WG_INTERFACE],
                check=True,
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
