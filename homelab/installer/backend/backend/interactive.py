"""Interactive wizard for YoLab installation."""

import sys

import questionary
from questionary import Style
from rich.progress import Progress, SpinnerColumn, TextColumn

from backend.display import (
    console,
    show_config_summary,
    show_disk_table,
    show_error,
    show_header,
    show_info,
    show_step,
    show_success,
    show_warning,
)
from backend.functions import (
    connect_wifi,
    detect_disks,
    run_installation,
    scan_wifi_networks,
    test_internet,
)
from backend.validators import (
    validate_git_url,
    validate_hostname,
    validate_ssh_key,
    validate_timezone,
)

# Custom questionary style
custom_style = Style(
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


class InteractiveInstaller:
    """Interactive installation wizard."""

    def __init__(self):
        self.config = {}
        self.available_disks = []

    def run(self) -> dict:
        """Run the interactive installation wizard."""
        show_header()

        # Step 1: Check internet connectivity
        if not self.check_internet():
            show_error("Installation cannot proceed without internet connectivity")
            sys.exit(1)

        # Step 2: Select disk
        show_step(1, 4, "Disk Selection")
        self.select_disk()

        # Step 3: System configuration
        show_step(2, 4, "System Configuration")
        self.configure_system()

        # Step 4: Review and confirm
        show_step(3, 4, "Review Configuration")
        if not self.review_and_confirm():
            show_warning("Installation cancelled by user")
            sys.exit(0)

        # Step 5: Run installation
        show_step(4, 4, "Installing NixOS")
        self.run_installation()

        return self.config

    def check_internet(self) -> bool:
        """Check internet connection and offer WiFi setup if needed."""
        console.print("[cyan]Checking internet connectivity...[/cyan]")

        if test_internet():
            show_success("Internet connection detected")
            return True

        show_warning("No internet connection detected")
        console.print()

        setup_wifi = questionary.confirm(
            "Would you like to set up WiFi?",
            default=True,
            style=custom_style,
        ).ask()

        if not setup_wifi:
            return False

        return self.setup_wifi()

    def setup_wifi(self) -> bool:
        """Interactive WiFi setup."""
        console.print()
        console.print("[yellow]Scanning for WiFi networks...[/yellow]")

        networks = scan_wifi_networks()

        if not networks:
            show_error("No WiFi networks found")
            return False

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
            style=custom_style,
        ).ask()

        if not selected_ssid:
            return False

        # Check if network is secured
        selected_network = next(n for n in networks if n["ssid"] == selected_ssid)
        needs_password = (
            selected_network["security"] and selected_network["security"] != "--"
        )

        password = ""
        if needs_password:
            password = questionary.password(
                "Enter WiFi password:",
                style=custom_style,
            ).ask()

            if password is None:
                return False

        console.print()
        console.print(f"[yellow]Connecting to {selected_ssid}...[/yellow]")

        if connect_wifi(selected_ssid, password):
            show_success(f"Connected to {selected_ssid}")

            # Store WiFi config for later use
            self.config["wifi_ssid"] = selected_ssid
            self.config["wifi_password"] = password

            # Verify internet connectivity
            console.print("[cyan]Verifying internet connection...[/cyan]")
            if test_internet():
                show_success("Internet connection verified")
                return True
            else:
                show_error("Connected to WiFi but no internet access")
                return False
        else:
            show_error("Failed to connect to WiFi")
            return False

    def select_disk(self):
        """Interactive disk selection."""
        self.available_disks = detect_disks()

        if not self.available_disks:
            show_error("No disks found")
            sys.exit(1)

        show_disk_table(self.available_disks)

        # Filter to only available (unmounted) disks
        available_disks = [d for d in self.available_disks if not d["mounted"]]

        if not available_disks:
            show_error("No available disks found (all disks are mounted)")
            sys.exit(1)

        # Create choices for questionary
        choices = [
            questionary.Choice(
                title=f"{disk['name']} ({disk['size']})", value=disk["name"]
            )
            for disk in available_disks
        ]

        selected_disk = questionary.select(
            "Select disk for installation:",
            choices=choices,
            style=custom_style,
        ).ask()

        if not selected_disk:
            show_error("No disk selected")
            sys.exit(1)

        self.config["disk"] = selected_disk

    def configure_system(self):
        """Prompt for system configuration."""
        console.print()

        # Hostname
        hostname = questionary.text(
            "Hostname:",
            default="homelab",
            validate=lambda text: validate_hostname(text)
            or "Invalid hostname (3-20 alphanumeric chars with hyphens)",
            style=custom_style,
        ).ask()

        if not hostname:
            show_error("Hostname is required")
            sys.exit(1)

        self.config["hostname"] = hostname

        # Timezone
        timezone = questionary.text(
            "Timezone:",
            default="UTC",
            validate=lambda text: validate_timezone(text) or "Invalid timezone format",
            style=custom_style,
        ).ask()

        if not timezone:
            show_error("Timezone is required")
            sys.exit(1)

        self.config["timezone"] = timezone

        # SSH Key - Choice between generate or provide
        console.print()
        key_choice = questionary.select(
            "SSH Key Setup:",
            choices=[
                questionary.Choice("Generate a new SSH key for me", value="generate"),
                questionary.Choice("I have my own SSH public key", value="provide"),
            ],
            style=custom_style,
        ).ask()

        if not key_choice:
            show_error("SSH key setup is required")
            sys.exit(1)

        if key_choice == "generate":
            self._generate_and_display_ssh_key()
        else:
            self._prompt_for_ssh_key()

        # Git Remote
        console.print()
        git_remote = questionary.text(
            "Git remote URL:",
            validate=lambda text: validate_git_url(text)
            or "Invalid git URL (must be http, https, or git protocol)",
            style=custom_style,
        ).ask()

        if not git_remote:
            show_error("Git remote URL is required")
            sys.exit(1)

        self.config["git_remote"] = git_remote

    def _generate_and_display_ssh_key(self):
        """Generate SSH key pair and display for user to save."""
        from backend.display import show_generated_ssh_key
        from backend.ssh_keygen import generate_ssh_keypair

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
                    style=custom_style,
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

            # Store the public key for installation
            self.config["root_ssh_key"] = public_key

        except Exception as e:
            show_error(f"Failed to generate SSH key: {e}")
            sys.exit(1)

    def _prompt_for_ssh_key(self):
        """Prompt user to paste their SSH public key."""
        console.print()
        show_info("Enter your SSH public key (paste and press Enter twice when done):")

        ssh_key_lines = []
        while True:
            line = questionary.text(
                "",
                style=custom_style,
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

        self.config["root_ssh_key"] = ssh_key

    def review_and_confirm(self) -> bool:
        """Show configuration summary and get confirmation."""
        show_config_summary(self.config)

        console.print(
            "[bold red]WARNING:[/bold red] This will ERASE all data on the selected disk!"
        )
        console.print()

        confirmed = questionary.confirm(
            "Proceed with installation?",
            default=False,
            style=custom_style,
        ).ask()

        return confirmed if confirmed is not None else False

    def run_installation(self):
        """Execute the installation with progress indication."""
        console.print()

        try:
            with Progress(
                SpinnerColumn(),
                TextColumn("[progress.description]{task.description}"),
                console=console,
            ) as progress:
                task = progress.add_task("Installing NixOS...", total=None)

                run_installation(
                    disk=self.config["disk"],
                    hostname=self.config["hostname"],
                    timezone=self.config["timezone"],
                    root_ssh_key=self.config["root_ssh_key"],
                    git_remote=self.config["git_remote"],
                )

                progress.update(task, completed=True)

            console.print()
            show_success("Installation completed successfully!")
            console.print()
            show_info("Remove installation media and reboot")
            console.print()

        except Exception as e:
            console.print()
            show_error(f"Installation failed: {e}")
            sys.exit(1)
