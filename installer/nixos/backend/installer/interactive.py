import os
import subprocess
import sys
import time
from pathlib import Path

import httpx
import qrcode

from installer.display import (
    console,
    show_error,
    show_info,
    show_step,
    show_success,
    show_header,
)
from installer.wireguard_live import register_and_bring_up_tunnel

INTERNAL_PORT = 8080
PLATFORM_API = "https://api.demycode.ovh"


def _create_account() -> str:
    resp = httpx.post(f"{PLATFORM_API}/users", timeout=15)
    resp.raise_for_status()
    return resp.json()["account_token"]


def _show_qr(url: str) -> None:
    qr = qrcode.QRCode(border=1)
    qr.add_data(url)
    qr.make(fit=True)
    console.print()
    console.print("[bold]Scan to open the installer on your phone:[/bold]")
    console.print(f"[dim]{url}[/dim]")
    console.print()
    qr.print_ascii(invert=True)
    console.print()


def _wait_for_web_ui() -> None:
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            httpx.get(f"http://127.0.0.1:{INTERNAL_PORT}/", timeout=2)
            return
        except Exception:
            time.sleep(1)
    show_error("Web UI did not start in time")
    sys.exit(1)


def _configure_caddy(dns_url: str) -> None:
    domain = dns_url.removeprefix("https://").removeprefix("http://").rstrip("/")
    frontend_path = os.environ.get("INSTALLER_FRONTEND_PATH", "")
    if not frontend_path:
        raise RuntimeError("INSTALLER_FRONTEND_PATH is not set")

    config = f"""{domain} {{
    handle /api/* {{
        reverse_proxy 127.0.0.1:{INTERNAL_PORT}
    }}
    handle {{
        root * {frontend_path}
        try_files {{path}} /index.html
        file_server
    }}
}}
"""
    caddy_dir = Path("/etc/caddy")
    caddy_dir.mkdir(parents=True, exist_ok=True)
    (caddy_dir / "installer.caddy").write_text(config)
    subprocess.run(["systemctl", "reload-or-restart", "caddy"], check=True)


def _push_tunnel_to_ui(tunnel: dict) -> None:
    try:
        httpx.post(
            f"http://127.0.0.1:{INTERNAL_PORT}/api/tunnel",
            json=tunnel,
            timeout=5,
        )
    except Exception as e:
        show_error(f"Could not push tunnel info to web UI: {e}")
        sys.exit(1)


def run_interactive_install() -> None:
    show_header()

    # ── Step 1: Create account ────────────────────────────────────────────────
    show_step(1, 3, "Account")
    console.print("[yellow]Creating account…[/yellow]")
    try:
        account_token = _create_account()
    except Exception as e:
        show_error(f"Could not reach YoLab platform: {e}")
        sys.exit(1)
    show_success("Account created.")

    # ── Step 2: WireGuard tunnel ──────────────────────────────────────────────
    show_step(2, 3, "Tunnel")
    console.print("[yellow]Registering installer tunnel…[/yellow]")
    try:
        tunnel = register_and_bring_up_tunnel(account_token, service_name="yolab")
    except Exception as e:
        show_error(f"Tunnel registration failed: {e}")
        sys.exit(1)

    dns_url = tunnel["dns_url"]

    # ── Step 3: Configure Caddy + show QR ─────────────────────────────────────
    show_step(3, 3, "Web Setup")
    console.print("[yellow]Configuring Caddy…[/yellow]")
    try:
        _configure_caddy(dns_url)
    except Exception as e:
        show_error(f"Caddy configuration failed: {e}")
        sys.exit(1)

    console.print("[yellow]Waiting for web UI…[/yellow]")
    _wait_for_web_ui()
    _push_tunnel_to_ui(tunnel)

    _show_qr(dns_url)
    show_info(f"Or open manually: {dns_url}")
    console.print()
