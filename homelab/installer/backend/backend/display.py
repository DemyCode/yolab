"""Display utilities using Rich for the YoLab installer."""

from rich.console import Console
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

console = Console()


def show_header():
    """Display the application header banner."""
    title = Text("YoLab Homelab Installer", style="bold cyan", justify="center")
    subtitle = Text("NixOS Installation Wizard", style="dim", justify="center")

    header = Text()
    header.append(title)
    header.append("\n")
    header.append(subtitle)

    panel = Panel(
        header,
        border_style="cyan",
        padding=(1, 2),
    )

    console.print()
    console.print(panel)
    console.print()


def show_step(step: int, total: int, description: str):
    """Display a step indicator."""
    step_text = f"[bold cyan]Step {step}/{total}:[/bold cyan] {description}"
    console.print()
    console.print(step_text)
    console.print()


def show_disk_table(disks: list[dict]):
    """Display available disks in a formatted table."""
    if not disks:
        console.print("[yellow]No disks found[/yellow]")
        return

    table = Table(show_header=True, header_style="bold magenta", border_style="dim")
    table.add_column("Device", style="cyan")
    table.add_column("Size", justify="right")
    table.add_column("Status")

    for disk in disks:
        status = "[red]Mounted[/red]" if disk["mounted"] else "[green]Available[/green]"
        table.add_row(
            disk["name"],
            disk["size"],
            status,
        )

    console.print(table)
    console.print()


def show_config_summary(config: dict):
    """Display the final configuration summary."""
    console.print()
    console.print("[bold cyan]Installation Configuration Summary:[/bold cyan]")
    console.print()

    table = Table(show_header=False, border_style="cyan", box=None, padding=(0, 2))
    table.add_column("Setting", style="bold")
    table.add_column("Value")

    table.add_row("Disk", f"[yellow]{config['disk']}[/yellow]")
    table.add_row("Hostname", config["hostname"])
    table.add_row("Timezone", config["timezone"])
    table.add_row(
        "SSH Key",
        f"{config['root_ssh_key'][:50]}..."
        if len(config["root_ssh_key"]) > 50
        else config["root_ssh_key"],
    )
    table.add_row("Git Remote", config["git_remote"])

    if config.get("wifi_ssid"):
        table.add_row("WiFi SSID", config["wifi_ssid"])
        table.add_row("WiFi Password", "[dim]********[/dim]")

    console.print(table)
    console.print()


def show_error(message: str):
    """Display an error message."""
    console.print(f"[bold red]✗ Error:[/bold red] {message}")


def show_success(message: str):
    """Display a success message."""
    console.print(f"[bold green]✓[/bold green] {message}")


def show_warning(message: str):
    """Display a warning message."""
    console.print(f"[bold yellow]⚠[/bold yellow] {message}")


def show_info(message: str):
    """Display an info message."""
    console.print(f"[cyan]→[/cyan] {message}")
