#!/usr/bin/env python3
import subprocess
from typing import Optional

from installer.install_flow import build_install_config, install_system
from installer.password import hash_password
from installer.ssh_keygen import generate_ssh_keypair
import typer
from rich import print as rprint
from rich.console import Console
from rich.table import Table
from typer import Typer

from installer.functions import get_status

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
    yes: bool = typer.Option(False, "--yes", "-y", help="Skip confirmation prompt"),
):
    """Install NixOS to disk.

    Only --disk and --password are required. All other options have sensible defaults.

    Examples:
      yolab-installer install --disk /dev/vda --password MySecurePass123
      yolab-installer install -d /dev/sda -p MyPass --hostname myserver --timezone America/New_York
    """

    required_args = [disk, password]
    has_all_required = all(arg is not None for arg in required_args)
    has_any_args = disk is not None or password is not None

    if interactive and not has_any_args:
        from installer.interactive import run_interactive_install

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
        )

        # Install system
        install_system(config)

    except subprocess.CalledProcessError as e:
        rprint(f"[bold red]✗ Installation failed:[/bold red] {e}")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@cli.command("wsl-setup")
def cli_wsl_setup():
    """Configure YoLab inside NixOS-WSL (no disk setup, no reboot required).

    Prompts for homelab settings and tunnel registration, writes config.toml,
    then applies the yolab-wsl NixOS configuration.
    """
    from pathlib import Path
    from installer.install_flow import (
        prompt_hostname,
        prompt_timezone,
        prompt_tunnel_setup,
        write_config_toml,
    )
    from installer.display import show_success, show_info

    rprint("[bold cyan]YoLab WSL Setup[/bold cyan]")
    rprint("[dim]Configuring your homelab inside NixOS-WSL...[/dim]\n")

    hostname = prompt_hostname()
    timezone = prompt_timezone()
    tunnel = prompt_tunnel_setup()

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
            "platform": "wsl",
            "flake_target": "yolab-wsl",
            "repo_path": "/etc/nixos",
        },
        "tunnel": tunnel if tunnel is not None else {"enabled": False},
    }

    config_path = Path("/etc/nixos/homelab/ignored/config.toml")
    show_info(f"Writing configuration to {config_path}")
    write_config_toml(config, config_path)
    show_success("Configuration written.")


if __name__ == "__main__":
    cli()
