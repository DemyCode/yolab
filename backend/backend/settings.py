from pathlib import Path

from devtools import pprint
from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict


class RegistrationAPISettings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=str(Path(__file__).parent.parent / ".env"),
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
        cli_parse_args=False,  
    )

    domain: str
    frps_server_ipv4: str  
    frps_server_port: int = Field(ge=1, le=65535)
    ipv6_subnet_base: str  # Base IPv6 subnet (e.g., "2a01:4f8:1c19:b96::")
    database_url: str
    port: int

    def model_post_init(self, __context):
        pprint(self)


settings = RegistrationAPISettings()  # ty: ignore[missing-argument]
