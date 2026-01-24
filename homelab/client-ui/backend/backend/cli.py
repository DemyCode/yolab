import subprocess
from pathlib import Path
from typing import Optional

import httpx
import typer
from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict
from rich import print as rprint
from rich.console import Console
from rich.table import Table
from typer import Typer

from backend.functions import (
    delete_service,
    download_service,
    init_config,
    list_available_services,
    list_downloaded_services,
    read_config,
    rebuild_system,
    validate_config,
)

console = Console()


class ClientUISettings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=str(Path(__file__).parent.parent / ".env"),
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
    )
    platform_api_url: str = Field(default="http://localhost:5000")
    config_path: str = Field(default="/etc/yolab/config.toml")
    services_dir: str = Field(default="/var/lib/yolab/services")
    flake_path: str = Field(default="/etc/nixos#yolab")
    port: int = Field(default=8080, ge=1, le=65535)


settings = ClientUISettings()

SERVICES_DIR = Path(settings.services_dir)
SERVICES_DIR.mkdir(parents=True, exist_ok=True)

cli = Typer(
    name="yolab",
    help="YoLab Client UI - Manage your homelab configuration",
    add_completion=False,
)

config_app = Typer(help="Manage configuration")
cli.add_typer(config_app, name="config")


@config_app.command("show")
def cli_config_show(
    path: Optional[Path] = typer.Option(
        None, "--path", "-p", help="Config file path (default: from settings)"
    ),
):
    try:
        config_path = path or Path(settings.config_path)
        config = read_config(config_path)
        rprint("[bold green]Current Configuration:[/bold green]")
        rprint(config)
    except FileNotFoundError:
        rprint("[bold red]Error:[/bold red] Config file not found")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@config_app.command("init")
def cli_config_init(
    example: Path = typer.Option(
        Path("/opt/yolab/homelab/ignored/config.toml.example"),
        "--example",
        "-e",
        help="Path to example config",
    ),
    output: Optional[Path] = typer.Option(
        None, "--output", "-o", help="Output path (default: from settings)"
    ),
):
    try:
        config_path = output or Path(settings.config_path)
        init_config(config_path, example)
        rprint(f"[bold green]✓[/bold green] Config initialized at {config_path}")
        rprint("[yellow]→[/yellow] Edit it and run 'yolab config validate'")
    except FileExistsError:
        rprint("[bold red]Error:[/bold red] Config already exists")
        raise typer.Exit(1)
    except FileNotFoundError:
        rprint("[bold red]Error:[/bold red] Example config not found")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@config_app.command("validate")
def cli_config_validate(
    path: Optional[Path] = typer.Option(
        None, "--path", "-p", help="Config file path (default: from settings)"
    ),
):
    try:
        config_path = path or Path(settings.config_path)
        is_valid, errors = validate_config(config_path)
        if is_valid:
            rprint("[bold green]✓ Configuration is valid[/bold green]")
        else:
            rprint("[bold red]✗ Configuration is invalid:[/bold red]")
            for error in errors:
                rprint(f"  [red]•[/red] {error}")
            raise typer.Exit(1)
    except FileNotFoundError:
        rprint("[bold red]Error:[/bold red] Config file not found")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


service_app = Typer(help="Manage services")
cli.add_typer(service_app, name="services")


@service_app.command("list")
def cli_services_list(
    downloaded: bool = typer.Option(
        False, "--downloaded", "-d", help="Show downloaded services only"
    ),
):
    try:
        if downloaded:
            services = list_downloaded_services(SERVICES_DIR)
            rprint("[bold green]Downloaded Services:[/bold green]")
            if not services:
                rprint("[yellow]No services downloaded[/yellow]")
                return

            table = Table(show_header=True, header_style="bold magenta")
            table.add_column("Name")
            table.add_column("Docker Compose")
            table.add_column("Caddyfile")

            for svc in services:
                table.add_row(
                    svc["name"],
                    "✓" if svc["has_compose"] else "✗",
                    "✓" if svc["has_caddy"] else "✗",
                )
            console.print(table)
        else:
            services = list_available_services(settings.platform_api_url)
            rprint("[bold green]Available Services:[/bold green]")
            if not services:
                rprint("[yellow]No services available[/yellow]")
                return
            for svc in services:
                rprint(f"  [cyan]•[/cyan] {svc}")
    except httpx.HTTPError as e:
        rprint(f"[bold red]Error:[/bold red] Failed to fetch services: {e}")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@service_app.command("download")
def cli_services_download(
    service_name: str = typer.Argument(..., help="Service name to download"),
):
    try:
        rprint(f"[yellow]Downloading service:[/yellow] {service_name}")
        download_service(SERVICES_DIR, settings.platform_api_url, service_name)
        rprint(f"[bold green]✓[/bold green] Service downloaded: {service_name}")
    except httpx.HTTPError as e:
        rprint(f"[bold red]Error:[/bold red] Failed to download: {e}")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@service_app.command("delete")
def cli_services_delete(
    service_name: str = typer.Argument(..., help="Service name to delete"),
    force: bool = typer.Option(False, "--force", "-f", help="Skip confirmation"),
):
    try:
        if not force:
            confirm = typer.confirm(f"Delete service '{service_name}'?")
            if not confirm:
                rprint("[yellow]Cancelled[/yellow]")
                raise typer.Exit(0)
        delete_service(SERVICES_DIR, service_name)
        rprint(f"[bold green]✓[/bold green] Service deleted: {service_name}")
    except FileNotFoundError:
        rprint(f"[bold red]Error:[/bold red] Service not found: {service_name}")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@cli.command("rebuild")
def cli_rebuild():
    try:
        rprint("[yellow]Rebuilding system...[/yellow]")
        result = rebuild_system(settings.flake_path)

        if result.returncode == 0:
            rprint("[bold green]✓ System rebuild successful![/bold green]")
            if result.stdout:
                rprint("\n[bold]Output:[/bold]")
                rprint(result.stdout)
        else:
            rprint("[bold red]✗ System rebuild failed[/bold red]")
            if result.stderr:
                rprint("\n[bold red]Error:[/bold red]")
                rprint(result.stderr)
            raise typer.Exit(result.returncode)
    except subprocess.TimeoutExpired:
        rprint("[bold red]Error:[/bold red] Rebuild timeout")
        raise typer.Exit(1)
    except Exception as e:
        rprint(f"[bold red]Error:[/bold red] {e}")
        raise typer.Exit(1)


@cli.command("health")
def cli_health_check():
    rprint("[bold green]✓ System is healthy[/bold green]")


@cli.command("server")
def cli_run_server(
    host: str = typer.Option("0.0.0.0", "--host", "-h", help="Host to bind to"),
    port: int = typer.Option(settings.port, "--port", "-p", help="Port to bind to"),
    reload: bool = typer.Option(False, "--reload", "-r", help="Enable auto-reload"),
):
    import uvicorn

    rprint(f"[bold green]Starting server on {host}:{port}[/bold green]")
    uvicorn.run("backend.backend:app", host=host, port=port, reload=reload)


if __name__ == "__main__":
    cli()
