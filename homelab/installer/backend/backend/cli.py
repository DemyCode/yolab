#!/usr/bin/env python3
import subprocess
from typing import Optional

import typer
from rich import print as rprint
from rich.console import Console
from rich.table import Table
from typer import Typer

from backend.functions import get_status, scan_wifi, wifi_connect

console = Console()

cli = Typer(
    name="yolab-installer",
    help="YoLab Installer - Install NixOS for homelab",
    add_completion=False,
)


@cli.command("status")
def cli_status():
    status = get_status()

    rprint("[bold green]System Status:[/bold green]")
    rprint(f"Internet: {'✓ Connected' if status['internet'] else '✗ Not connected'}")
    rprint("\n[bold green]Available Disks:[/bold green]")

    if not status["disks"]:
        rprint("[yellow]No disks found[/yellow]")
        return

    table = Table(show_header=True, header_style="bold magenta")
    table.add_column("Device")
    table.add_column("Size")
    table.add_column("Mounted")

    for disk in status["disks"]:
        table.add_row(
            disk["name"],
            disk["size"],
            "✓" if disk["mounted"] else "✗",
        )
    console.print(table)


wifi_app = Typer(help="WiFi management")
cli.add_typer(wifi_app, name="wifi")


@wifi_app.command("scan")
def cli_wifi_scan():
    rprint("[yellow]Scanning WiFi networks...[/yellow]")
    result = scan_wifi()
    networks = result["networks"]

    if not networks:
        rprint("[yellow]No networks found[/yellow]")
        return

    rprint("[bold green]Available Networks:[/bold green]")
    table = Table(show_header=True, header_style="bold magenta")
    table.add_column("SSID")
    table.add_column("Signal")
    table.add_column("Security")

    for network in networks:
        table.add_row(
            network["ssid"],
            network["signal"],
            network["security"],
        )
    console.print(table)


@wifi_app.command("connect")
def cli_wifi_connect(
    ssid: str = typer.Argument(..., help="WiFi SSID to connect to"),
    password: Optional[str] = typer.Option(
        None, "--password", "-p", help="WiFi password"
    ),
):
    try:
        rprint(f"[yellow]Connecting to {ssid}...[/yellow]")
        result = wifi_connect(ssid, password or "")
        rprint(f"[bold green]✓[/bold green] {result['message']}")
    except Exception as e:
        rprint(f"[bold red]✗[/bold red] {e}")
        raise typer.Exit(1)


@cli.command("install")
def cli_install(
    interactive: bool = typer.Option(
        True, "--interactive/--no-interactive", help="Run interactive wizard"
    ),
    disk: Optional[str] = typer.Option(
        None, "--disk", "-d", help="Disk to install on (e.g. /dev/vda) [REQUIRED]"
    ),
    password: Optional[str] = typer.Option(
        None, "--password", "-p", help="Homelab user password [REQUIRED]"
    ),
    hostname: Optional[str] = typer.Option(
        "yolab", "--hostname", "-h", help="System hostname (default: yolab)"
    ),
    timezone: Optional[str] = typer.Option(
        "UTC", "--timezone", "-t", help="Timezone (default: UTC)"
    ),
    root_ssh_key: Optional[str] = typer.Option(
        "",
        "--ssh-key",
        "-s",
        help="Root SSH public key (default: none, will auto-generate)",
    ),
    git_remote: Optional[str] = typer.Option(
        "https://github.com/DemyCode/yolab.git",
        "--git-remote",
        "-g",
        help="Git remote URL (default: official repo)",
    ),
    wifi_ssid: Optional[str] = typer.Option(
        None, "--wifi-ssid", help="WiFi SSID (optional)"
    ),
    wifi_password: Optional[str] = typer.Option(
        None, "--wifi-password", help="WiFi password (optional)"
    ),
    yes: bool = typer.Option(False, "--yes", "-y", help="Skip confirmation prompt"),
):
    """Install NixOS to disk.

    Only --disk and --password are required. All other options have sensible defaults.

    Examples:
      yolab-installer install --disk /dev/vda --password MySecurePass123
      yolab-installer install -d /dev/sda -p MyPass --hostname myserver --timezone America/New_York
    """
    from backend.install_flow import build_install_config, install_system
    from backend.password import hash_password
    from backend.ssh_keygen import generate_ssh_keypair

    # Only disk and password are truly required
    required_args = [disk, password]
    has_all_required = all(arg is not None for arg in required_args)
    has_any_args = disk is not None or password is not None

    # If interactive mode and no args, run wizard
    if interactive and not has_any_args:
        from backend.interactive import run_interactive_install

        run_interactive_install()
        return

    # If some but not all required args provided, show error
    if has_any_args and not has_all_required:
        rprint(
            "[bold red]Error:[/bold red] Missing required arguments for non-interactive install"
        )
        rprint("\n[yellow]Required:[/yellow] --disk and --password")
        rprint("[yellow]Or run without arguments for interactive mode[/yellow]")
        rprint("\n[dim]Example:[/dim]")
        rprint(
            "[dim]  yolab-installer install --disk /dev/vda --password MySecurePass123[/dim]"
        )
        raise typer.Exit(1)

    # Non-interactive mode - check required args
    if not has_all_required:
        rprint("[bold red]Error:[/bold red] --disk and --password are required")
        raise typer.Exit(1)

    # Generate SSH key if not provided
    if not root_ssh_key or root_ssh_key == "":
        rprint("[yellow]No SSH key provided, generating one...[/yellow]")
        try:
            private_key, public_key = generate_ssh_keypair()
            root_ssh_key = public_key
            rprint("[bold green]✓[/bold green] SSH key generated")
            rprint("\n[bold yellow]⚠ SAVE YOUR SSH PRIVATE KEY:[/bold yellow]")
            rprint("[dim]─" * 60 + "[/dim]")
            rprint(private_key)
            rprint("[dim]─" * 60 + "[/dim]")
            rprint("[yellow]Save this key to a file and use it to connect:[/yellow]")
            rprint("[dim]  ssh -i your-key.pem root@your-server[/dim]\n")
        except Exception as e:
            rprint(f"[bold red]✗[/bold red] Failed to generate SSH key: {e}")
            raise typer.Exit(1)

    # Set up WiFi if credentials provided
    if wifi_ssid:
        try:
            rprint(f"[yellow]Connecting to WiFi network {wifi_ssid}...[/yellow]")
            wifi_connect(wifi_ssid, wifi_password or "")
            rprint("[bold green]✓[/bold green] Connected to WiFi")
        except Exception as e:
            rprint(f"[bold red]✗[/bold red] WiFi connection failed: {e}")
            raise typer.Exit(1)

    # Show configuration
    rprint("[bold green]Installation Configuration:[/bold green]")
    rprint(f"  Disk: {disk}")
    rprint(f"  Hostname: {hostname}")
    rprint(f"  Timezone: {timezone}")
    rprint(f"  Git Remote: {git_remote}")
    rprint(
        f"  SSH Key: {'Generated' if root_ssh_key and not root_ssh_key == '' else 'Provided'}"
    )
    rprint()

    # Confirm unless --yes flag
    if not yes:
        confirm = typer.confirm("Proceed with installation? This will ERASE the disk!")
        if not confirm:
            rprint("[yellow]Installation cancelled[/yellow]")
            raise typer.Exit(0)

    try:
        rprint("[yellow]Starting installation...[/yellow]")

        # Hash password (type assertion since we verified it's not None)
        assert password is not None
        password_hash = hash_password(password)

        # Build configuration (type assertions since we have defaults)
        assert disk is not None
        assert hostname is not None
        assert timezone is not None
        assert root_ssh_key is not None
        assert git_remote is not None

        config = build_install_config(
            disk=disk,
            hostname=hostname,
            timezone=timezone,
            root_ssh_key=root_ssh_key,
            git_remote=git_remote,
            homelab_password_hash=password_hash,
            wifi_ssid=wifi_ssid,
            wifi_password=wifi_password,
        )

        # Install system
        install_system(config)

    except subprocess.CalledProcessError as e:
        rprint(f"[bold red]✗ Installation failed:[/bold red] {e}")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


if __name__ == "__main__":
    cli()
