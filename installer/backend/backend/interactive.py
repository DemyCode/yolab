"""Interactive installation wizard - orchestrates prompts and installation."""

import sys

import questionary

from backend.display import (
    console,
    show_config_summary,
    show_error,
    show_header,
    show_step,
    show_warning,
)
from backend.install_flow import (
    PROMPT_STYLE,
    build_install_config,
    check_internet_connectivity,
    install_system,
    prompt_disk_selection,
    prompt_git_remote,
    prompt_hostname,
    prompt_password,
    prompt_ssh_key_setup,
    prompt_timezone,
    prompt_tunnel_setup,
    setup_wifi_interactive,
)


def run_interactive_install() -> None:
    show_header()

    if not check_internet_connectivity():
        console.print()
        setup_wifi = questionary.confirm(
            "Would you like to set up WiFi?",
            default=True,
            style=PROMPT_STYLE,
        ).ask()

        if not setup_wifi:
            show_error("Installation cannot proceed without internet connectivity")
            sys.exit(1)

        wifi_config = setup_wifi_interactive()
        if not wifi_config:
            show_error("Installation cannot proceed without internet connectivity")
            sys.exit(1)

        wifi_ssid = wifi_config["ssid"]
        wifi_password = wifi_config["password"]
    else:
        wifi_ssid = None
        wifi_password = None

    show_step(1, 5, "Disk Selection")
    disk = prompt_disk_selection()

    show_step(2, 5, "System Configuration")
    console.print()

    hostname = prompt_hostname()
    timezone = prompt_timezone()
    root_ssh_key = prompt_ssh_key_setup()

    console.print()
    git_remote = prompt_git_remote()

    console.print()
    homelab_password_hash = prompt_password()

    show_step(3, 5, "Tunnel Registration")
    tunnel = prompt_tunnel_setup()

    config = build_install_config(
        disk=disk,
        hostname=hostname,
        timezone=timezone,
        root_ssh_key=root_ssh_key,
        git_remote=git_remote,
        homelab_password_hash=homelab_password_hash,
        tunnel=tunnel,
        wifi_ssid=wifi_ssid,
        wifi_password=wifi_password,
    )

    show_step(4, 5, "Review Configuration")
    show_config_summary(config)

    console.print(
        "[bold red]WARNING:[/bold red] This will ERASE all data on the selected disk!"
    )
    console.print()

    confirmed = questionary.confirm(
        "Proceed with installation?",
        default=False,
        style=PROMPT_STYLE,
    ).ask()

    if not confirmed:
        show_warning("Installation cancelled by user")
        sys.exit(0)

    show_step(5, 5, "Installing NixOS")
    install_system(config)
