import ipaddress
import subprocess
import sys
from pathlib import Path

from installer.display import show_error, show_success
from installer.wg_keygen import generate_wg_keypair

PLATFORM_API = "https://api.demycode.ovh"


def register_and_bring_up_tunnel(account_token: str, service_name: str) -> dict:
    import httpx

    private_key, public_key = generate_wg_keypair()

    # Step 1: create the WireGuard tunnel (allocates IPv6, no DNS)
    resp = httpx.post(
        f"{PLATFORM_API}/tunnels",
        json={"account_token": account_token, "wg_public_key": public_key},
        timeout=15,
    )
    resp.raise_for_status()
    tunnel_data = resp.json()

    tunnel_id = tunnel_data["tunnel_id"]
    sub_ipv6 = tunnel_data["sub_ipv6"]
    wg_server_endpoint = tunnel_data["wg_server_endpoint"]
    wg_server_public_key = tunnel_data["wg_server_public_key"]

    # Step 2: attach an AAAA record so the management domain resolves
    record_resp = httpx.post(
        f"{PLATFORM_API}/tunnels/{tunnel_id}/records",
        json={
            "account_token": account_token,
            "record_type": "AAAA",
            "name": service_name,
            "value": sub_ipv6,
        },
        timeout=15,
    )
    record_resp.raise_for_status()
    dns_url = f"https://{record_resp.json()['fqdn']}"

    node_resp = httpx.post(
        f"{PLATFORM_API}/nodes",
        json={"account_token": account_token, "wg_public_key": public_key},
        timeout=15,
    )
    node_resp.raise_for_status()
    node_info = node_resp.json()

    sub_ipv6_private = node_info["sub_ipv6"]
    node_id = node_info["node_id"]

    # Derive the /112 subnet that covers all node cluster IPs from this
    # node's own private address.  All nodes are allocated sequentially
    # from the same /112 base, so masking to /112 gives the shared subnet
    # that the NixOS WireGuard postSetup needs for destination routing.
    sub_ipv6_private_subnet = str(
        ipaddress.ip_network(f"{sub_ipv6_private}/112", strict=False)
    )

    # Table = off: disable wg-quick's automatic route injection so the
    # installer's own outbound traffic (DNS, package downloads) is NOT
    # routed through the tunnel.
    #
    # PostUp adds a policy rule: packets *sourced from* sub_ipv6 (i.e.
    # return traffic for inbound connections) use routing table 51820,
    # which sends everything through wg0. This keeps the return path
    # working without hijacking the installer's own internet traffic.
    conf = (
        f"[Interface]\n"
        f"PrivateKey = {private_key}\n"
        f"Address = {sub_ipv6}/128\n"
        f"Table = off\n"
        f"PostUp = ip -6 rule add from {sub_ipv6} lookup 51820 priority 100; "
        f"ip -6 route add ::/0 dev wg0 table 51820\n"
        f"PreDown = ip -6 rule del from {sub_ipv6} lookup 51820 priority 100; "
        f"ip -6 route del ::/0 dev wg0 table 51820\n\n"
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
        "tunnel_id": tunnel_id,
        "node_id": node_id,
        "wg_private_key": private_key,
        "wg_public_key": public_key,
        "sub_ipv6": sub_ipv6,
        "sub_ipv6_private": sub_ipv6_private,
        "sub_ipv6_private_subnet": sub_ipv6_private_subnet,
        "dns_url": dns_url,
        "wg_server_endpoint": wg_server_endpoint,
        "wg_server_public_key": wg_server_public_key,
    }
