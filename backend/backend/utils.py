import ipaddress
import secrets

from sqlmodel import Session, select

from backend.models import IPv6Counter, Service, User
from backend.settings import settings


def allocate_ipv6(db: Session) -> str:
    """Allocate IPv6 address using atomic counter to prevent race conditions.

    Uses singleton pattern with id=1 to ensure only one counter exists.
    The SELECT FOR UPDATE lock prevents concurrent allocations.
    """
    # Get the singleton counter (id=1) with row-level lock
    # This prevents race conditions by locking the row during the transaction
    counter_obj = db.exec(
        select(IPv6Counter).where(IPv6Counter.id == 1).with_for_update()
    ).first()

    if not counter_obj:
        # Should never happen if migration ran correctly, but handle it gracefully
        counter_obj = IPv6Counter(id=1, counter=0)
        db.add(counter_obj)
        db.flush()

    # Increment counter atomically
    counter_obj.counter += 1
    next_id = counter_obj.counter
    db.add(counter_obj)
    db.flush()

    # Calculate IPv6 address from base + counter
    base = ipaddress.IPv6Address(settings.ipv6_subnet_base)
    allocated = base + next_id

    return str(allocated)


def generate_frpc_config(service: Service, user: User) -> str:
    """Generate FRP client configuration for a service."""
    return "\n".join(
        [
            "[common]",
            f"server_addr = {settings.frps_server_ipv6}",
            f"server_port = {settings.frps_server_port}",
            f"user = service_{service.id}",
            f"meta_account_token = {user.account_token}",
            f"meta_service_id = {service.id}",
            "",
            f"[{service.service_name}]",
            f"type = {service.service_type.value}",
            "local_ip = 127.0.0.1",
            f"local_port = {service.local_port}",
            f"remote_ip = {service.ipv6_address}",
            f"remote_port = {service.remote_port}",
        ]
    )


def get_service_access_url(service: Service, user: User) -> tuple[str, str]:
    """Get subdomain and direct IPv6 access URLs for a service."""
    subdomain_url = f"{service.subdomain}.{settings.domain}"
    direct_url = f"[{service.ipv6_address}]:{service.remote_port}"
    return (subdomain_url, direct_url)


def generate_account_token() -> str:
    """Generate a secure random account token."""
    return secrets.token_urlsafe(24)
