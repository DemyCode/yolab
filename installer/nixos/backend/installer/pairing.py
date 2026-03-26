import sys
import time

import httpx
import qrcode

from installer.display import console, show_error, show_success

PLATFORM_API = "https://api.demycode.ovh"
POLL_INTERVAL = 3
POLL_TIMEOUT = 600


def create_pairing_session() -> tuple[str, str]:
    resp = httpx.post(f"{PLATFORM_API}/pairing", timeout=15)
    resp.raise_for_status()
    data = resp.json()
    return data["session_id"], data["pair_url"]


def show_qr(url: str, label: str) -> None:
    qr = qrcode.QRCode(border=1)
    qr.add_data(url)
    qr.make(fit=True)
    console.print()
    console.print(f"[bold]{label}[/bold]")
    console.print(f"[dim]{url}[/dim]")
    console.print()
    qr.print_ascii(invert=True)
    console.print()


def poll_for_account_token(session_id: str) -> str:
    deadline = time.time() + POLL_TIMEOUT
    while time.time() < deadline:
        try:
            resp = httpx.get(f"{PLATFORM_API}/pairing/{session_id}", timeout=10)
            if resp.status_code == 200:
                data = resp.json()
                if data.get("status") == "linked":
                    return data["account_token"]
        except Exception:
            pass
        console.print("[dim]Waiting for account link…[/dim]", end="\r")
        time.sleep(POLL_INTERVAL)

    show_error("Timed out waiting for account link")
    sys.exit(1)


def set_installer_url(session_id: str, installer_url: str) -> None:
    resp = httpx.post(
        f"{PLATFORM_API}/pairing/{session_id}/installer-url",
        json={"installer_url": installer_url},
        timeout=10,
    )
    resp.raise_for_status()


def acquire_account_token() -> tuple[str, str]:
    console.print("[yellow]Creating pairing session…[/yellow]")
    try:
        session_id, pair_url = create_pairing_session()
    except Exception as e:
        show_error(f"Could not reach YoLab platform: {e}")
        sys.exit(1)

    show_qr(pair_url, "Scan with your phone to sign in or create an account:")
    console.print("[dim]Waiting for you to complete sign-in on your device…[/dim]")

    token = poll_for_account_token(session_id)
    console.print()
    show_success("Account linked!")
    return session_id, token
