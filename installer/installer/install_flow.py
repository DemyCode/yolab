import secrets
import subprocess
import sys
import uuid
from pathlib import Path

import questionary
import tomli_w
from questionary import Style

from installer.display import (
    console,
    show_disk_table,
    show_error,
    show_generated_ssh_key,
    show_info,
    show_success,
    show_warning,
)
from installer.functions import (
    detect_disks,
    detect_ram_size,
)
from installer.password import hash_password, validate_password_strength
from installer.ssh_keygen import generate_ssh_keypair
from installer.tunnel import register_tunnel
from installer.validators import (
    validate_git_url,
    validate_hostname,
    validate_ssh_key,
    validate_timezone,
)
from installer.wg_keygen import generate_wg_keypair

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

def generate_node_id() -> str:
    return str(uuid.uuid4())


def _parse_size(size_str: str) -> float:
    size_str = size_str.strip().upper()
    if size_str.endswith("T"):
        return float(size_str[:-1]) * 1000
    if size_str.endswith("G"):
        return float(size_str[:-1])
    if size_str.endswith("M"):
        return float(size_str[:-1]) / 1000
    return 0.0


def auto_select_disk() -> str:
    available_disks = detect_disks()
    if not available_disks:
        show_error("No disks found")
        sys.exit(1)
    unmounted = [d for d in available_disks if not d["mounted"]]
    if not unmounted:
        show_error("No unmounted disks found")
        sys.exit(1)
    return max(unmounted, key=lambda d: _parse_size(d.get("size", "0")))["name"]


def prompt_disk_selection() -> str:
    available_disks = detect_disks()

    if not available_disks:
        show_error("No disks found")
        sys.exit(1)

    show_disk_table(available_disks)

    unmounted_disks = [d for d in available_disks if not d["mounted"]]

    if not unmounted_disks:
        show_error("No available disks found (all disks are mounted)")
        sys.exit(1)

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


def prompt_cluster_setup() -> dict:
    console.print()
    is_first = questionary.confirm(
        "Is this the first machine in your cluster?",
        default=True,
        style=PROMPT_STYLE,
    ).ask()

    if is_first:
        token = secrets.token_urlsafe(32)
        show_success("This node will be the first K3s server.")
        show_info(f"Cluster token (save this for joining other nodes): {token}")
        return {"enabled": True, "k3s": {"role": "server", "token": token, "server_addr": ""}}

    server_url = questionary.text(
        "K3s server address (https://[ipv6]:6443):",
        style=PROMPT_STYLE,
    ).ask()

    token = questionary.text(
        "Cluster token:",
        style=PROMPT_STYLE,
    ).ask()

    if not server_url or not token:
        show_error("Server address and token are required to join a cluster")
        return {"enabled": False, "k3s": {"role": "server", "token": "", "server_addr": ""}}

    return {"enabled": True, "k3s": {"role": "agent", "token": token, "server_addr": server_url}}


# ============================================================================
# System Configuration Prompts
# ============================================================================


def prompt_hostname() -> str:
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


def prompt_tunnel_setup(platform_api_url: str | None = None) -> dict | None:
    console.print()
    wants_tunnel = questionary.confirm(
        "Register a YoLab tunnel so your homelab is reachable from the internet?",
        default=True,
        style=PROMPT_STYLE,
    ).ask()

    if not wants_tunnel:
        return None

    if not platform_api_url:
        platform_api_url = questionary.text(
            "YoLab platform API URL:",
            default="http://188.245.104.63:5000",
            style=PROMPT_STYLE,
        ).ask()

    if not platform_api_url:
        show_error("Platform API URL is required for tunnel registration")
        return None

    account_token = questionary.text(
        "Your account token:",
        style=PROMPT_STYLE,
    ).ask()

    if not account_token:
        show_error("Account token is required")
        return None

    service_name = questionary.text(
        "Service name (identifies this homelab):",
        default="homelab",
        style=PROMPT_STYLE,
    ).ask()

    if not service_name:
        show_error("Service name is required")
        return None

    console.print("[yellow]Generating WireGuard key pair...[/yellow]")
    try:
        wg_private_key, wg_public_key = generate_wg_keypair()
    except Exception as e:
        show_error(f"Failed to generate WireGuard keys: {e}")
        return None

    console.print("[yellow]Registering tunnel with YoLab platform...[/yellow]")
    try:
        result = register_tunnel(
            platform_api_url, account_token, service_name, wg_public_key
        )
    except Exception as e:
        show_error(f"Tunnel registration failed: {e}")
        return None

    show_success(f"Tunnel registered — IPv6: {result['sub_ipv6']}")

    return {
        "enabled": True,
        "platform_api_url": platform_api_url,
        "account_token": account_token,
        "service_name": service_name,
        "service_id": result["service_id"],
        "wg_private_key": wg_private_key,
        "wg_public_key": wg_public_key,
        "sub_ipv6": result["sub_ipv6"],
        "wg_server_endpoint": result["wg_server_endpoint"],
        "wg_server_public_key": result["wg_server_public_key"],
    }


def build_install_config(
    disk: str,
    hostname: str,
    timezone: str,
    root_ssh_key: str,
    git_remote: str,
    homelab_password_hash: str,
    tunnel: dict | None = None,
    swarm: dict | None = None,
    node_id: str | None = None,
) -> dict:
    swap_size = detect_ram_size()

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
        "docker": {
            "enabled": False,
            "compose_url": "",
        },
        "tunnel": tunnel if tunnel is not None else {"enabled": False},
        "swarm": {"enabled": swarm.get("enabled", False)} if swarm else {"enabled": False},
        "node": {
            "node_id": node_id or generate_node_id(),
            "k3s": swarm.get("k3s", {"role": "server", "token": "", "server_addr": ""}) if swarm else {},
        },
    }

    return config


def write_config_toml(config: dict, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        tomli_w.dump(config, f)


def install_system(config: dict) -> None:
    import shutil

    code_dir = Path("yolab")

    if code_dir.exists():
        shutil.rmtree(code_dir)

    code_dir.mkdir(parents=True, exist_ok=True)

    console.print("[yellow]Cloning homelab repository...[/yellow]")
    subprocess.run(
        [
            "git",
            "clone",
            config["homelab"]["git_remote"],
            str(code_dir),
        ],
        check=True,
    )
    console.print(f"[dim]Repository cloned to: {code_dir}[/dim]")

    console.print("[yellow]Writing configuration...[/yellow]")
    config_path = code_dir / "homelab" / "ignored" / "config.toml"
    write_config_toml(config, config_path)

    console.print("[yellow]Generating hardware configuration...[/yellow]")
    hardware_config = code_dir / "homelab" / "ignored" / "hardware-configuration.nix"
    result = subprocess.run(
        ["nixos-generate-config", "--no-filesystems", "--show-hardware-config"],
        check=True,
        capture_output=True,
        text=True,
    )
    hardware_config.write_text(result.stdout)

    console.print(
        "[yellow]Step 1: Partitioning and formatting disk with disko...[/yellow]"
    )
    console.print(f"[dim]Target disk: {config['disk']['device']}[/dim]")
    console.print("[dim]This will erase all data on the disk...[/dim]")
    console.print()

    disk_config_path = code_dir / "homelab" / "nixos" / "disk-config.nix"
    subprocess.run(
        [
            "disko",
            "--yes-wipe-all-disks",
            "--mode",
            "destroy,format,mount",
            str(disk_config_path),
        ],
        check=True,
    )

    console.print()
    show_success("Disk partitioned and mounted to /mnt")
    console.print()

    console.print("[yellow]Step 3: Installing NixOS...[/yellow]")
    console.print()

    subprocess.run(
        [
            "nixos-install",
            "--flake",
            f"path:{code_dir}#yolab",
            "--no-root-password",
            "--verbose",
        ],
        check=True,
    )

    subprocess.run(["rsync", "-a", f"{str(code_dir)}/", "/mnt/etc/nixos"], check=True)

    console.print()
    show_success("Installation completed successfully!")
    console.print()
    show_info("Remove installation media and reboot")
    console.print()
