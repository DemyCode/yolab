#!/usr/bin/env python3
import subprocess
from typing import Optional

import typer
from rich import print as rprint
from rich.console import Console
from rich.table import Table
from typer import Typer

from backend.functions import get_status, install, scan_wifi, wifi_connect

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
    disk: str = typer.Option(
        ..., "--disk", "-d", help="Disk to install on (e.g. /dev/sda)"
    ),
    hostname: str = typer.Option(..., "--hostname", "-h", help="System hostname"),
    timezone: str = typer.Option(..., "--timezone", "-t", help="Timezone (e.g. UTC)"),
    root_ssh_key: str = typer.Option(
        ..., "--ssh-key", "-s", help="Root SSH public key"
    ),
    git_remote: str = typer.Option(..., "--git-remote", "-g", help="Git remote URL"),
):
    rprint("[bold green]Installation Configuration:[/bold green]")
    rprint(f"  Disk: {disk}")
    rprint(f"  Hostname: {hostname}")
    rprint(f"  Timezone: {timezone}")
    rprint(f"  Git Remote: {git_remote}")
    rprint()

    confirm = typer.confirm("Proceed with installation? This will ERASE the disk!")
    if not confirm:
        rprint("[yellow]Installation cancelled[/yellow]")
        raise typer.Exit(0)

    try:
        rprint("[yellow]Starting installation...[/yellow]")
        result = install(disk, hostname, timezone, root_ssh_key, git_remote)
        rprint(f"[bold green]✓ {result['message']}[/bold green]")
        rprint("[yellow]→[/yellow] Remove installation media and reboot")
    except subprocess.CalledProcessError as e:
        rprint(f"[bold red]✗ Installation failed:[/bold red] {e}")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@cli.command("server")
def cli_run_server(
    host: str = typer.Option("0.0.0.0", "--host", "-h", help="Host to bind to"),
    port: int = typer.Option(8000, "--port", "-p", help="Port to bind to"),
):
    import uvicorn

    rprint(f"[bold green]Starting server on {host}:{port}[/bold green]")
    uvicorn.run("main:app", host=host, port=port)


if __name__ == "__main__":
    cli()
