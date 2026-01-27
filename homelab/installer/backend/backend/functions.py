import json
import subprocess
from pathlib import Path


def test_internet() -> bool:
    try:
        result = subprocess.run(
            ["ping", "-c", "1", "-W", "2", "1.1.1.1"],
            capture_output=True,
            timeout=3,
        )
        return result.returncode == 0
    except (subprocess.TimeoutExpired, OSError):
        return False


def scan_wifi_networks() -> list[dict]:
    try:
        subprocess.run(
            ["nmcli", "device", "wifi", "rescan"], capture_output=True, timeout=10
        )
        result = subprocess.run(
            ["nmcli", "-t", "-f", "SSID,SIGNAL,SECURITY", "device", "wifi", "list"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        networks = []
        for line in result.stdout.strip().split("\n"):
            if line:
                parts = line.split(":")
                if len(parts) >= 3 and parts[0]:
                    networks.append(
                        {
                            "ssid": parts[0],
                            "signal": parts[1],
                            "security": parts[2],
                        }
                    )
        return sorted(
            networks, key=lambda x: int(x["signal"]) if x["signal"] else 0, reverse=True
        )
    except (subprocess.TimeoutExpired, OSError, json.JSONDecodeError):
        return []


def connect_wifi(ssid: str, password: str) -> bool:
    try:
        if password:
            result = subprocess.run(
                ["nmcli", "device", "wifi", "connect", ssid, "password", password],
                capture_output=True,
                text=True,
                timeout=30,
            )
        else:
            result = subprocess.run(
                ["nmcli", "device", "wifi", "connect", ssid],
                capture_output=True,
                text=True,
                timeout=30,
            )
        return result.returncode == 0
    except (subprocess.TimeoutExpired, OSError):
        return False


def get_wifi_config() -> dict | None:
    try:
        result = subprocess.run(
            ["nmcli", "-t", "-f", "NAME,TYPE", "connection", "show", "--active"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        for line in result.stdout.strip().split("\n"):
            if "802-11-wireless" in line:
                ssid = line.split(":")[0]
                psk_result = subprocess.run(
                    [
                        "nmcli",
                        "-s",
                        "-g",
                        "802-11-wireless-security.psk",
                        "connection",
                        "show",
                        ssid,
                    ],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                return {"ssid": ssid, "psk": psk_result.stdout.strip()}
        return None
    except (subprocess.TimeoutExpired, OSError):
        return None


def detect_disks() -> list[dict]:
    try:
        result = subprocess.run(
            ["lsblk", "-J", "-o", "NAME,SIZE,TYPE,MOUNTPOINT"],
            capture_output=True,
            text=True,
            check=True,
        )
        data = json.loads(result.stdout)
        disks = []
        for device in data.get("blockdevices", []):
            if device.get("type") == "disk":
                disks.append(
                    {
                        "name": f"/dev/{device['name']}",
                        "size": device["size"],
                        "mounted": bool(device.get("mountpoint")),
                    }
                )
        return disks
    except (subprocess.CalledProcessError, json.JSONDecodeError, OSError):
        return []


def detect_ram_size() -> int:
    try:
        with open("/proc/meminfo", "r") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    kb = int(line.split()[1])
                    gb = kb // (1024 * 1024)
                    return min(gb, 32)
    except (OSError, ValueError, IndexError):
        return 8
    return 8


def generate_config_toml(
    disk: str,
    hostname: str,
    timezone: str,
    root_ssh_key: str,
    swap_size: int,
    git_remote: str,
    wifi_config: dict | None,
    homelab_password_hash: str | None = None,
) -> str:
    wifi_section = ""
    if wifi_config:
        wifi_section = f'''[wifi]
enabled = true
ssid = "{wifi_config["ssid"]}"
psk = "{wifi_config["psk"]}"

'''

    password_line = ""
    if homelab_password_hash:
        password_line = f'\nhomelab_password_hash = "{homelab_password_hash}"'

    return f'''[homelab]
hostname = "{hostname}"
timezone = "{timezone}"
locale = "en_US.UTF-8"
ssh_port = 22
root_ssh_key = "{root_ssh_key}"
git_remote = "{git_remote}"
allowed_ssh_keys = []{password_line}

[disk]
device = "{disk}"
esp_size = "500M"
swap_size = "{swap_size}G"

{wifi_section}[client_ui]
enabled = true
port = 8080
platform_api_url = ""

[docker]
enabled = false
compose_url = ""

[frpc]
enabled = false
server_addr = ""
server_port = 7000
account_token = ""
'''


def run_installation(
    disk: str,
    hostname: str,
    timezone: str,
    root_ssh_key: str,
    git_remote: str,
    homelab_password_hash: str | None = None,
) -> None:
    import shutil

    install_dir = Path("/mnt/installer")

    if install_dir.exists():
        shutil.rmtree(install_dir)

    install_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        ["git", "clone", git_remote, str(install_dir)],
        check=True,
        capture_output=True,
    )

    swap_size = detect_ram_size()
    wifi_config = get_wifi_config()

    config_toml = install_dir / "ignored" / "config.toml"
    config_toml.parent.mkdir(parents=True, exist_ok=True)
    config_toml.write_text(
        generate_config_toml(
            disk,
            hostname,
            timezone,
            root_ssh_key,
            swap_size,
            git_remote,
            wifi_config,
            homelab_password_hash,
        )
    )

    hardware_config = install_dir / "ignored" / "hardware-configuration.nix"
    result = subprocess.run(
        ["nixos-generate-config", "--no-filesystems", "--show-hardware-config"],
        check=True,
        capture_output=True,
        text=True,
    )
    hardware_config.write_text(result.stdout)

    subprocess.run(
        [
            "nix",
            "--extra-experimental-features",
            "nix-command flakes",
            "--verbose",
            "--print-build-logs",
            "--show-trace",
            "run",
            "github:nix-community/disko/latest#disko-install",
            "--",
            "--flake",
            f"{install_dir}#yolab",
            "--disk",
            "disk1",
            disk,
            "--write-efi-boot-entries",
        ],
        check=True,
    )

    nixos_dir = Path("/mnt/etc/nixos")
    nixos_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        ["cp", "-rT", str(install_dir), str(nixos_dir)],
        check=True,
        capture_output=True,
    )


def get_status() -> dict:
    return {
        "internet": test_internet(),
        "disks": detect_disks(),
    }


def scan_wifi() -> dict:
    return {"networks": scan_wifi_networks()}


def wifi_connect(ssid: str, password: str) -> dict:
    success = connect_wifi(ssid, password)
    if not success:
        raise Exception("Failed to connect to WiFi")
    return {"success": True, "message": f"Connected to {ssid}"}


def install(
    disk: str, hostname: str, timezone: str, root_ssh_key: str, git_remote: str
) -> dict:
    if not test_internet():
        raise Exception("Internet connection required")

    run_installation(disk, hostname, timezone, root_ssh_key, git_remote)

    return {
        "success": True,
        "message": "Installation complete",
        "hostname": hostname,
        "disk": disk,
        "git_remote": git_remote,
    }
