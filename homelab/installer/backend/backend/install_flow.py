"""Core installation flow - reusable functions for both interactive and non-interactive modes."""

import subprocess
import sys
from pathlib import Path

import questionary
import tomli_w
from questionary import Style

from backend.display import (
    console,
    show_disk_table,
    show_error,
    show_generated_ssh_key,
    show_info,
    show_success,
    show_warning,
)
from backend.functions import (
    connect_wifi,
    detect_disks,
    detect_ram_size,
    get_wifi_config,
    scan_wifi_networks,
    test_internet,
)
from backend.password import hash_password, validate_password_strength
from backend.ssh_keygen import generate_ssh_keypair
from backend.validators import (
    validate_git_url,
    validate_hostname,
    validate_ssh_key,
    validate_timezone,
)

# Custom questionary style
PROMPT_STYLE = Style(
    [
        ("qmark", "fg:#00ff00 bold"),
        ("question", "bold"),
        ("answer", "fg:#00ff00 bold"),
        ("pointer", "fg:#00ff00 bold"),
        ("highlighted", "fg:#00ff00 bold"),
        ("selected", "fg:#00ff00"),
        ("separator", "fg:#666666"),
        ("instruction", "fg:#666666"),
        ("text", ""),
        ("disabled", "fg:#666666 italic"),
    ]
)


# ============================================================================
# Internet and WiFi
# ============================================================================


def check_internet_connectivity() -> bool:
    """Check if internet is available."""
    console.print("[cyan]Checking internet connectivity...[/cyan]")
    if test_internet():
        show_success("Internet connection detected")
        return True
    else:
        show_warning("No internet connection detected")
        return False


def setup_wifi_interactive() -> dict | None:
    """
    Interactive WiFi setup.
    Returns wifi config dict or None if setup failed/skipped.
    """
    console.print()
    networks = scan_wifi_networks()

    if not networks:
        show_error("No WiFi networks found")
        return None

    # Create choices for questionary
    choices = [
        questionary.Choice(
            title=f"{net['ssid']} ({net['signal']}% - {net['security']})",
            value=net["ssid"],
        )
        for net in networks
    ]

    selected_ssid = questionary.select(
        "Select WiFi network:",
        choices=choices,
        style=PROMPT_STYLE,
    ).ask()

    if not selected_ssid:
        return None

    # Check if network is secured
    selected_network = next(n for n in networks if n["ssid"] == selected_ssid)
    needs_password = (
        selected_network["security"] and selected_network["security"] != "--"
    )

    password = ""
    if needs_password:
        password = questionary.password(
            "Enter WiFi password:",
            style=PROMPT_STYLE,
        ).ask()

        if password is None:
            return None

    console.print()
    console.print(f"[yellow]Connecting to {selected_ssid}...[/yellow]")

    if connect_wifi(selected_ssid, password):
        show_success(f"Connected to {selected_ssid}")

        # Verify internet connectivity
        console.print("[cyan]Verifying internet connection...[/cyan]")
        if test_internet():
            show_success("Internet connection verified")
            return {"ssid": selected_ssid, "password": password}
        else:
            show_error("Connected to WiFi but no internet access")
            return None
    else:
        show_error("Failed to connect to WiFi")
        return None


# ============================================================================
# Disk Selection
# ============================================================================


def prompt_disk_selection() -> str:
    """
    Interactive disk selection.
    Returns selected disk path.
    """
    available_disks = detect_disks()

    if not available_disks:
        show_error("No disks found")
        sys.exit(1)

    show_disk_table(available_disks)

    # Filter to only available (unmounted) disks
    unmounted_disks = [d for d in available_disks if not d["mounted"]]

    if not unmounted_disks:
        show_error("No available disks found (all disks are mounted)")
        sys.exit(1)

    # Create choices for questionary
    choices = [
        questionary.Choice(title=f"{disk['name']} ({disk['size']})", value=disk["name"])
        for disk in unmounted_disks
    ]

    selected_disk = questionary.select(
        "Select disk for installation:",
        choices=choices,
        style=PROMPT_STYLE,
    ).ask()

    if not selected_disk:
        show_error("No disk selected")
        sys.exit(1)

    return selected_disk


# ============================================================================
# System Configuration Prompts
# ============================================================================


def prompt_hostname() -> str:
    """Prompt for hostname."""
    hostname = questionary.text(
        "Hostname:",
        default="homelab",
        validate=lambda text: validate_hostname(text)
        or "Invalid hostname (3-20 alphanumeric chars with hyphens)",
        style=PROMPT_STYLE,
    ).ask()

    if not hostname:
        show_error("Hostname is required")
        sys.exit(1)

    return hostname


def prompt_timezone() -> str:
    """Prompt for timezone."""
    timezone = questionary.text(
        "Timezone:",
        default="UTC",
        validate=lambda text: validate_timezone(text) or "Invalid timezone format",
        style=PROMPT_STYLE,
    ).ask()

    if not timezone:
        show_error("Timezone is required")
        sys.exit(1)

    return timezone


def prompt_ssh_key_setup() -> str:
    """
    Prompt for SSH key setup (generate or provide).
    Returns public key.
    """
    console.print()
    key_choice = questionary.select(
        "SSH Key Setup:",
        choices=[
            questionary.Choice("Generate a new SSH key for me", value="generate"),
            questionary.Choice("I have my own SSH public key", value="provide"),
        ],
        style=PROMPT_STYLE,
    ).ask()

    if not key_choice:
        show_error("SSH key setup is required")
        sys.exit(1)

    if key_choice == "generate":
        return generate_and_display_ssh_key()
    else:
        return prompt_ssh_public_key()


def generate_and_display_ssh_key() -> str:
    """Generate SSH key pair, display private key, return public key."""
    console.print()
    console.print("[yellow]Generating SSH key pair...[/yellow]")

    try:
        private_key, public_key = generate_ssh_keypair()

        console.print()
        show_generated_ssh_key(private_key)

        # Confirm they saved it
        saved = False
        while not saved:
            saved = questionary.confirm(
                "Have you saved your SSH private key securely?",
                default=False,
                style=PROMPT_STYLE,
            ).ask()

            if saved is None:
                show_error("You must confirm you have saved your key")
                sys.exit(1)

            if not saved:
                console.print()
                show_warning("Please save your key before continuing!")
                console.print()
                console.print("[dim]Scroll up to see your private key[/dim]")
                console.print()

        return public_key

    except Exception as e:
        show_error(f"Failed to generate SSH key: {e}")
        sys.exit(1)


def prompt_ssh_public_key() -> str:
    """Prompt user to paste their SSH public key."""
    console.print()
    show_info("Enter your SSH public key (paste and press Enter twice when done):")

    ssh_key_lines = []
    while True:
        line = questionary.text(
            "",
            style=PROMPT_STYLE,
        ).ask()

        if line is None:
            show_error("SSH key is required")
            sys.exit(1)

        if not line.strip():
            if ssh_key_lines:
                break
            else:
                continue

        ssh_key_lines.append(line.strip())

    ssh_key = " ".join(ssh_key_lines)

    if not validate_ssh_key(ssh_key):
        show_error(
            "Invalid SSH key format (must start with ssh-ed25519, ssh-rsa, or ecdsa-sha2-*)"
        )
        sys.exit(1)

    return ssh_key


def prompt_git_remote() -> str:
    """Prompt for git remote URL."""
    git_remote = questionary.text(
        "Git remote URL:",
        default="https://github.com/DemyCode/yolab.git",
        validate=lambda text: validate_git_url(text)
        or "Invalid git URL (must be http, https, or git protocol)",
        style=PROMPT_STYLE,
    ).ask()

    if not git_remote:
        show_error("Git remote URL is required")
        sys.exit(1)

    return git_remote


def prompt_password() -> str:
    """
    Prompt for homelab user password.
    Returns hashed password.
    """
    console.print("[bold cyan]Set password for homelab user[/bold cyan]")
    console.print(
        "[dim]This password will be required for sudo commands (e.g., nixos-rebuild)[/dim]"
    )
    console.print()

    while True:
        password = questionary.password(
            "Enter password:",
            style=PROMPT_STYLE,
        ).ask()

        if not password:
            show_error("Password is required")
            sys.exit(1)

        # Validate password strength
        is_valid, error_msg = validate_password_strength(password)
        if not is_valid:
            show_error(error_msg)
            console.print()
            continue

        # Confirm password
        password_confirm = questionary.password(
            "Confirm password:",
            style=PROMPT_STYLE,
        ).ask()

        if not password_confirm:
            show_error("Password confirmation is required")
            sys.exit(1)

        if password != password_confirm:
            show_error("Passwords do not match")
            console.print()
            continue

        # Hash the password
        password_hash = hash_password(password)
        show_success("Password set successfully")
        return password_hash


# ============================================================================
# Config Building
# ============================================================================


def build_install_config(
    disk: str,
    hostname: str,
    timezone: str,
    root_ssh_key: str,
    git_remote: str,
    homelab_password_hash: str,
    wifi_ssid: str | None = None,
    wifi_password: str | None = None,
) -> dict:
    """
    Build complete installation configuration dictionary.

    Args:
        disk: Disk device path (e.g., /dev/sda)
        hostname: System hostname
        timezone: Timezone string
        root_ssh_key: SSH public key for root
        git_remote: Git repository URL
        homelab_password_hash: Hashed password for homelab user
        wifi_ssid: WiFi SSID (optional)
        wifi_password: WiFi password (optional)

    Returns:
        Complete config dictionary ready for installation
    """
    swap_size = detect_ram_size()
    wifi_config = get_wifi_config()

    config = {
        "homelab": {
            "hostname": hostname,
            "timezone": timezone,
            "locale": "en_US.UTF-8",
            "ssh_port": 22,
            "root_ssh_key": root_ssh_key,
            "git_remote": git_remote,
            "allowed_ssh_keys": [],
            "homelab_password_hash": homelab_password_hash,
        },
        "disk": {
            "device": disk,
            "esp_size": "500M",
            "swap_size": f"{swap_size}G",
        },
        "client_ui": {
            "enabled": False,
            "port": 8080,
            "platform_api_url": "",
        },
        "docker": {
            "enabled": False,
            "compose_url": "",
        },
        "frpc": {
            "enabled": False,
            "server_addr": "",
            "server_port": 7000,
            "account_token": "",
        },
    }

    # Add WiFi config if available
    if wifi_config or (wifi_ssid and wifi_password):
        if wifi_ssid and wifi_password:
            # Use provided WiFi credentials
            config["wifi"] = {
                "enabled": True,
                "ssid": wifi_ssid,
                "psk": wifi_password,
            }
        elif wifi_config:
            # Use existing WiFi connection
            config["wifi"] = {
                "enabled": True,
                "ssid": wifi_config["ssid"],
                "psk": wifi_config["psk"],
            }

    return config


def write_config_toml(config: dict, path: Path) -> None:
    """
    Write configuration dictionary to TOML file.

    Args:
        config: Configuration dictionary
        path: Path to write TOML file
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        tomli_w.dump(config, f)


# ============================================================================
# Installation
# ============================================================================


def install_system(config: dict) -> None:
    """
    Install NixOS using the provided configuration.

    Args:
        config: Complete installation configuration dictionary
    """
    import shutil

    install_dir = Path("/mnt/installer")

    # Clean up existing installation directory
    if install_dir.exists():
        shutil.rmtree(install_dir)

    install_dir.mkdir(parents=True, exist_ok=True)

    # Clone repository
    console.print("[yellow]Cloning homelab repository...[/yellow]")
    subprocess.run(
        [
            "git",
            "clone",
            "--depth",
            "1",
            config["homelab"]["git_remote"],
            str(install_dir),
        ],
        check=True,
    )
    console.print(f"[dim]Repository cloned to: {install_dir}[/dim]")

    console.print("[yellow]Writing configuration...[/yellow]")
    config_path = install_dir / "homelab" / "ignored" / "config.toml"
    write_config_toml(config, config_path)

    console.print("[yellow]Generating hardware configuration...[/yellow]")
    hardware_config = install_dir / "homelab" / "ignored" / "hardware-configuration.nix"
    result = subprocess.run(
        ["nixos-generate-config", "--no-filesystems", "--show-hardware-config"],
        check=True,
        capture_output=True,
        text=True,
    )
    hardware_config.write_text(result.stdout)

    # Run disko-install
    console.print("[yellow]Running disko-install...[/yellow]")
    console.print(
        "[dim]This will partition disk, install NixOS, and set up boot entries...[/dim]"
    )
    console.print("[dim]This will take several minutes...[/dim]")
    console.print()

    subprocess.run(
        [
            "nix",
            "--extra-experimental-features",
            "nix-command flakes",
            "run",
            "github:nix-community/disko/latest#disko-install",
            "--",
            "--mode",
            "format",
            "--flake",
            f"path:{install_dir}/homelab#yolab",
            "--disk",
            "disk1",
            config["disk"]["device"],
            "--write-efi-boot-entries",
        ],
        check=True,
    )

    # Copy configuration to installed system
    console.print("[yellow]Copying configuration to installed system...[/yellow]")
    nixos_dir = Path("/mnt/etc/nixos")
    nixos_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        ["cp", "-rT", str(install_dir), str(nixos_dir)],
        check=True,
    )

    console.print()
    show_success("Installation completed successfully!")
    console.print()
    show_info("Remove installation media and reboot")
    console.print()
