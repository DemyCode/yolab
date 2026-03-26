import subprocess
import sys
from pathlib import Path

from installer.display import show_error, show_success
from installer.wg_keygen import generate_wg_keypair

PLATFORM_API = "https://api.demycode.ovh"


def register_and_bring_up_tunnel(account_token: str, service_name: str) -> dict:
    import httpx

    private_key, public_key = generate_wg_keypair()

    resp = httpx.post(
        f"{PLATFORM_API}/services",
        json={"account_token": account_token, "service_name": service_name, "wg_public_key": public_key},
        timeout=15,
    )
    resp.raise_for_status()
    info = resp.json()

    sub_ipv6 = info["sub_ipv6"]
    dns_url = info["dns_url"]
    wg_server_endpoint = info["wg_server_endpoint"]
    wg_server_public_key = info["wg_server_public_key"]
    service_id = info["service_id"]

    conf = (
        f"[Interface]\n"
        f"PrivateKey = {private_key}\n"
        f"Address = {sub_ipv6}/128\n\n"
        f"[Peer]\n"
        f"PublicKey = {wg_server_public_key}\n"
        f"Endpoint = {wg_server_endpoint}\n"
        f"AllowedIPs = ::/0\n"
        f"PersistentKeepalive = 25\n"
    )

    wg_conf = Path("/etc/wireguard/wg0.conf")
    wg_conf.parent.mkdir(parents=True, exist_ok=True)
    wg_conf.write_text(conf)
    wg_conf.chmod(0o600)

    result = subprocess.run(["wg-quick", "up", "wg0"], capture_output=True, text=True)
    if result.returncode != 0:
        show_error(f"wg-quick up failed: {result.stderr}")
        sys.exit(1)

    show_success(f"WireGuard tunnel up — {dns_url}")

    return {
        "enabled": True,
        "platform_api_url": PLATFORM_API,
        "account_token": account_token,
        "service_name": service_name,
        "service_id": service_id,
        "wg_private_key": private_key,
        "wg_public_key": public_key,
        "sub_ipv6": sub_ipv6,
        "dns_url": dns_url,
        "wg_server_endpoint": wg_server_endpoint,
        "wg_server_public_key": wg_server_public_key,
    }
