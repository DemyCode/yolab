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
    homelab_password_hash: str | None = None,
) -> str:
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
        ["git", "clone", "--depth", "1", git_remote, str(install_dir)],
        check=True,
    )

    # Check if repository has homelab subdirectory and adjust path
    homelab_subdir = install_dir / "homelab"
    if homelab_subdir.exists():
        flake_path = homelab_subdir / "flake.nix"
        if flake_path.exists():
            install_dir = homelab_subdir
        # If homelab/ exists but no flake, keep using install_dir root

    swap_size = detect_ram_size()

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
            homelab_password_hash,
        )
    )

    hardware_config = install_dir / "ignored" / "hardware-configuration.nix"
    result = subprocess.run(
        ["nixos-generate-config", "--no-filesystems", "--show-hardware-config"],
        check=True,
        capture_output=True,  # Need to capture stdout to write to file
        text=True,
    )
    hardware_config.write_text(result.stdout)

    # Step 1: Run disko to partition, format, and mount
    disk_config_path = install_dir / "nixos" / "disk-config.nix"
    subprocess.run(
        [
            "disko",
            "--mode",
            "destroy,format,mount",
            str(disk_config_path),
        ],
        check=True,
    )

    # Step 2: Copy configuration to mounted system
    nixos_dir = Path("/mnt/etc/yolab")
    nixos_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        ["cp", "-rT", str(install_dir), str(nixos_dir)],
        check=True,
    )

    # Step 3: Run nixos-install
    subprocess.run(
        [
            "nixos-install",
            "--flake",
            f"/mnt/etc/nixos/homelab#yolab",
            "--no-root-password",
            "--max-jobs",
            "1",
            "--cores",
            "2",
        ],
        check=True,
    )


def get_status() -> dict:
    return {
        "internet": test_internet(),
        "disks": detect_disks(),
    }

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
