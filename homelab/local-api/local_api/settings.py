from pathlib import Path

from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=str(Path(__file__).parent.parent / ".env"),
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
        cli_parse_args=False,
    )

    yolab_repo_path: str = "/etc/nixos"
    yolab_platform: str = "nixos"
    yolab_flake_target: str = "yolab"
    yolab_node_ipv6: str = "::1"
    yolab_config: str = "/etc/nixos/homelab/ignored/config.toml"
    port: int = 3001

    # Filesystem paths
    rebuild_log: Path = Path("/var/log/yolab-rebuild.log")
    rebuild_pid: Path = Path("/run/yolab-rebuild.pid")
    built_dir: Path = Path("/var/lib/yolab")
    osd_img_path: Path = Path("/var/lib/rook/system-osd.img")
    k3s_server_dir: Path = Path("/var/lib/rancher/k3s/server")

settings = Settings()
