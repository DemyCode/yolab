import shutil
import subprocess
import time
import uuid
from collections.abc import Callable
from pathlib import Path

import tomli_w

GIT_REMOTE = "https://github.com/DemyCode/yolab.git"


def generate_node_id() -> str:
    return str(uuid.uuid4())


def build_install_config(
    disk: str,
    hostname: str,
    timezone: str,
    root_ssh_key: str,
    homelab_password_hash: str,
    git_remote: str = GIT_REMOTE,
    tunnel: dict | None = None,
) -> dict:
    return {
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
        },
        "docker": {"enabled": False, "compose_url": ""},
        "tunnel": tunnel if tunnel is not None else {"enabled": False},
        "swarm": {"enabled": False},
        "node": {"node_id": generate_node_id(), "k3s": {}},
    }


def install_system(config: dict, log: Callable[[str], None] = lambda _: None) -> None:
    code_dir = Path("/tmp/yolab-install")

    if code_dir.exists():
        shutil.rmtree(code_dir)

    def run(cmd: list[str]) -> None:
        proc = subprocess.Popen(
            cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
        )
        assert proc.stdout
        for line in proc.stdout:
            log(line.rstrip())
        proc.wait()
        if proc.returncode != 0:
            raise RuntimeError(f"Command failed: {' '.join(cmd)}")

    log("Cloning repository…")
    run(["git", "clone", config["homelab"]["git_remote"], str(code_dir)])

    log("Writing config.toml…")
    config_path = code_dir / "homelab" / "ignored" / "config.toml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    with open(config_path, "wb") as f:
        tomli_w.dump(config, f)

    log("Generating hardware configuration…")
    result = subprocess.run(
        ["nixos-generate-config", "--no-filesystems", "--show-hardware-config"],
        capture_output=True,
        text=True,
        check=True,
    )
    (code_dir / "homelab" / "ignored" / "hardware-configuration.nix").write_text(
        result.stdout
    )

    log(f"Partitioning disk {config['disk']['device']} with disko…")
    disk_config = code_dir / "homelab" / "nixos" / "disk-config.nix"
    run(["disko", "--yes-wipe-all-disks", "--mode", "destroy,format,mount", str(disk_config)])

    log("Installing NixOS (this takes several minutes)…")
    run(["nixos-install", "--flake", f"path:{code_dir}#yolab", "--no-root-password"])

    log("Copying repository to installed system…")
    run(["rsync", "-a", f"{code_dir}/", "/mnt/etc/nixos"])

    log("Setting installed disk as next boot target…")
    run(["nixos-enter", "--root", "/mnt", "--", "bootctl", "set-oneshot", "@current"])

    log("Done! Rebooting in 10 seconds…")
    time.sleep(10)
    subprocess.run(["systemctl", "reboot"], check=False)
