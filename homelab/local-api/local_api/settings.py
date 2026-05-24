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
    system_storage_path: str = "/var/yolab-data"
    exports_file: Path = Path("/etc/exports.d/yolab.exports")


settings = Settings()
