import typer

cli = typer.Typer(
    name="yolab-installer",
    help="YoLab Installer",
    add_completion=False,
)


@cli.command("install")
def cli_install() -> None:
    """Pair account, register tunnel, and launch the web UI."""
    from installer.interactive import run_interactive_install

    run_interactive_install()


@cli.command("serve")
def cli_serve() -> None:
    """Run the installer web UI (called by the systemd service)."""
    from installer import web_ui

    web_ui.run()


if __name__ == "__main__":
    cli()
