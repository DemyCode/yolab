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
        cli_parse_args=False,  # Disabled to prevent conflicts with Alembic CLI
    )

    domain: str
    frps_server_ipv6: str
    frps_server_port: int = Field(ge=1, le=65535)
    ipv6_subnet_base: str
    database_url: str
    registration_api_host: str
    registration_api_port: int
    username_pattern: str = Field(default=r"^[a-z0-9-]{3,20}$")
    service_name_pattern: str = Field(default=r"^[a-z0-9-]{3,20}$")

    def model_post_init(self, __context):
        pprint(self)


settings = RegistrationAPISettings()  # ty: ignore[missing-argument]
