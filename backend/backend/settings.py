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

    domain: str
    wg_server_endpoint: str
    wg_server_public_key: str
    ipv6_subnet_base: str
    database_url: str
    port: int


settings = Settings()
