"""Tunnel registration with the YoLab platform API."""

import json
import urllib.error
import urllib.request


def register_tunnel(
    platform_api_url: str,
    account_token: str,
    service_name: str,
    wg_public_key: str,
) -> dict:
    """
    Register a WireGuard tunnel with the YoLab platform API.

    Returns a dict with: service_id, sub_ipv6, wg_server_endpoint, wg_server_public_key
    Raises an exception on failure.
    """
    url = f"{platform_api_url.rstrip('/')}/services"
    payload = json.dumps(
        {
            "account_token": account_token,
            "service_name": service_name,
            "wg_public_key": wg_public_key,
        }
    ).encode()

    req = urllib.request.Request(
        url,
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        body = e.read().decode(errors="replace")
        raise Exception(f"API error {e.code}: {body}") from e
