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
    port: int = 3001


settings = Settings()
