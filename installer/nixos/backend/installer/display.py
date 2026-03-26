from rich.console import Console
from rich.panel import Panel
from rich.text import Text

console = Console()


def show_header() -> None:
    header = Text()
    header.append(Text("YoLab Homelab Installer", style="bold cyan", justify="center"))
    header.append("\n")
    header.append(Text("NixOS Installation Wizard", style="dim", justify="center"))
    console.print()
    console.print(Panel(header, border_style="cyan", padding=(1, 2)))
    console.print()


def show_step(step: int, total: int, description: str) -> None:
    console.print()
    console.print(f"[bold cyan]Step {step}/{total}:[/bold cyan] {description}")
    console.print()


def show_error(message: str) -> None:
    console.print(f"[bold red]✗ Error:[/bold red] {message}")


def show_success(message: str) -> None:
    console.print(f"[bold green]✓[/bold green] {message}")


def show_warning(message: str) -> None:
    console.print(f"[bold yellow]⚠[/bold yellow] {message}")


def show_info(message: str) -> None:
    console.print(f"[cyan]→[/cyan] {message}")
