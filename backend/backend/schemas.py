
from pydantic import BaseModel


class TokenResponse(BaseModel):
    account_token: str


class RegisterRequest(BaseModel):
    account_token: str
    service_name: str
    wg_public_key: str


class RegisterResponse(BaseModel):
    service_id: int
    sub_ipv6: str
    wg_server_endpoint: str
    wg_server_public_key: str


class WireGuardPeer(BaseModel):
    sub_ipv6: str
    wg_public_key: str


class DNSResolveResponse(BaseModel):
    found: bool
    ipv6_address: str | None = None
