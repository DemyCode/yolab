import ipaddress
import secrets

from sqlmodel import Session, select

from backend.models import Service, User
from backend.settings import settings


def allocate_sub_ipv6(db: Session) -> str:
    used_ipv6s = set(service.sub_ipv6 for service in db.exec(select(Service)).all())
    base = ipaddress.IPv6Address(settings.ipv6_subnet_base.rstrip(":"))
    for i in range(1, 65536):
        candidate = str(base + i)
        if candidate not in used_ipv6s:
            return candidate
    raise RuntimeError("No available IPv6 addresses in subnet")


def allocate_frps_port(db: Session) -> int:
    used_ports = set(
        service.frps_internal_port for service in db.exec(select(Service)).all()
    )
    for port in range(10000, 65536):
        if port not in used_ports:
            return port
    raise RuntimeError("No available FRPS internal ports")


def generate_frpc_config(service: Service, user: User) -> str:
    """Generate FRP client configuration for a service."""
    return "\n".join(
        [
            "[common]",
            f"server_addr = {settings.frps_server_ipv4}",
            f"server_port = {settings.frps_server_port}",
            f"user = service_{service.id}",
            f"meta_account_token = {user.account_token}",
            f"meta_service_id = {service.id}",
            "",
            f"[{service.service_name}]",
            f"type = {service.service_type.value}",
            "local_ip = 127.0.0.1",
            f"local_port = {service.local_port}",
            f"remote_port = {service.frps_internal_port}",
        ]
    )


def get_service_access_url(service: Service, user: User) -> tuple[str, str]:
    """Get subdomain and direct IPv6 access URLs for a service."""
    subdomain_url = f"{service.subdomain}.{settings.domain}"
    direct_url = f"[{service.sub_ipv6}]:{service.client_port}"
    return (subdomain_url, direct_url)


def generate_account_token() -> str:
    """Generate a secure random account token."""
    return secrets.token_urlsafe(24)
