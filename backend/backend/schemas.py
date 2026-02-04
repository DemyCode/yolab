import re
from typing import Dict, List

from pydantic import BaseModel, Field, field_validator

from backend.models import ServiceType


class TokenResponse(BaseModel):
    account_token: str
    created_at: str


class RegisterRequest(BaseModel):
    account_token: str = Field(..., min_length=16, max_length=64)
    service_name: str = Field(..., min_length=3, max_length=20)
    service_type: str
    local_port: int = Field(..., ge=1, le=65535)
    client_port: int = Field(..., ge=1, le=65535)  # NEW: Port exposed to users

    @field_validator("service_name")
    @classmethod
    def validate_alphanumeric(cls, v):
        v = v.lower().strip()
        if not re.match(r"^[a-z0-9-]{3,20}$", v):
            raise ValueError(
                "must be 3-20 characters, lowercase letters, numbers, and hyphens only"
            )
        return v

    @field_validator("service_type")
    @classmethod
    def validate_service_type(cls, v):
        if v not in [t.value for t in ServiceType]:
            raise ValueError("Invalid service type. Must be: tcp or udp")
        return v


class ServiceResponse(BaseModel):
    service_id: int
    subdomain: str
    sub_ipv6: str  
    client_port: int  
    access_direct: str
    frpc_config: str

    class Config:
        from_attributes = True


class ServiceInfo(BaseModel):
    service_id: int
    service_name: str
    service_type: str
    subdomain: str
    sub_ipv6: str  # NEW
    client_port: int  # NEW
    local_port: int
    access_url: str
    access_direct: str
    created_at: str

    class Config:
        from_attributes = True


class UserDashboard(BaseModel):
    account_token: str
    services: List[ServiceInfo]

    class Config:
        from_attributes = True


# Service config
class ServiceConfigResponse(BaseModel):
    service_id: int
    service_name: str
    service_type: str
    subdomain: str
    ipv6_address: str
    remote_port: int
    local_port: int
    access_url: str
    access_direct: str
    frpc_config: str


class StatsResponse(BaseModel):
    total_users: int
    total_services: int
    tcp_services: int
    udp_services: int
    ipv6_subnet: str


class AvailableService(BaseModel):
    name: str
    has_docker_compose: bool
    has_caddyfile: bool


class AvailableServicesResponse(BaseModel):
    services: List[AvailableService]


class ServiceTemplateResponse(BaseModel):
    service_name: str
    docker_compose: str | None = None
    caddyfile: str | None = None


class AuthValidateRequest(BaseModel):
    account_token: str
    service_id: int
    proxy_type: str
    remote_ip: str
    remote_port: int


class AuthValidateResponse(BaseModel):
    valid: bool
    reason: str = ""
    service_id: int | None = None


class DNSResolveResponse(BaseModel):
    found: bool
    ipv6_address: str | None = None
    service_id: int | None = None
    fallback_to_main: bool = False


class LastSeenResponse(BaseModel):
    success: bool
    service_id: int


class PluginUser(BaseModel):
    user: str
    metas: Dict[str, str]
    run_id: str


class PluginContent(BaseModel):
    user: PluginUser
    proxy_name: str
    proxy_type: str
    use_encryption: bool
    use_compression: bool
    metas: Dict[str, str]
    remote_ip: str = ""
    remote_port: int = 0


class PluginRequest(BaseModel):
    version: str
    op: str
    content: PluginContent


class PluginRequestBody(BaseModel):
    version: str
    op: str
    content: PluginContent


class PluginResponse(BaseModel):
    reject: bool
    reject_reason: str
    unchange: bool


class NFTablesRule(BaseModel):
    """Single nftables rule for service routing."""

    service_id: int
    sub_ipv6: str
    client_port: int
    protocol: str  # tcp or udp
    frps_internal_port: int


class NFTablesRulesResponse(BaseModel):
    """Response containing all active nftables rules."""

    rules: List[NFTablesRule]
