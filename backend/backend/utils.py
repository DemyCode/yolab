import ipaddress
import secrets

from sqlmodel import Session, select

from backend.models import IPv6Counter, FRPSPortCounter, Service, User
from backend.settings import settings


def allocate_ipv6(db: Session) -> str:
    """Allocate IPv6 address using atomic counter to prevent race conditions.

    Uses singleton pattern with id=1 to ensure only one counter exists.
    The SELECT FOR UPDATE lock prevents concurrent allocations.

    DEPRECATED: Use allocate_sub_ipv6() instead for new architecture.
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


def allocate_sub_ipv6(db: Session) -> str:
    """Allocate IPv6 address from subnet using hex counter.

    Example: counter=10 -> "2a01:4f8:1c19:b96::a"
             counter=255 -> "2a01:4f8:1c19:b96::ff"

    Returns: Full IPv6 address as string
    """
    counter_obj = db.exec(
        select(IPv6Counter).where(IPv6Counter.id == 1).with_for_update()
    ).first()

    if not counter_obj:
        counter_obj = IPv6Counter(id=1, counter=0)
        db.add(counter_obj)
        db.flush()

    # Increment counter atomically
    counter_obj.counter += 1
    next_id = counter_obj.counter
    db.add(counter_obj)
    db.flush()

    # Format as hex and append to base subnet
    # Remove trailing :: if present, then add ::{counter:x}
    base = settings.ipv6_subnet_base.rstrip(":")
    allocated = f"{base}:{next_id:x}"

    return allocated


def allocate_frps_port(db: Session) -> int:
    """Allocate FRPS internal port using atomic counter.

    Starts at 38000 and increments for each new service.
    These ports are used internally by FRPS, not exposed to users.

    Returns: Port number (e.g., 38000, 38001, 38002, ...)
    """
    counter_obj = db.exec(
        select(FRPSPortCounter).where(FRPSPortCounter.id == 1).with_for_update()
    ).first()

    if not counter_obj:
        counter_obj = FRPSPortCounter(id=1, counter=38000)
        db.add(counter_obj)
        db.flush()

    # Get current port and increment
    port = counter_obj.counter
    counter_obj.counter += 1
    db.add(counter_obj)
    db.flush()

    return port


def generate_frpc_config(service: Service, user: User) -> str:
    """Generate FRP client configuration for a service.

    Uses frps_internal_port for remote_port (FRPS backend port).
    Client connects to FRPS via IPv4 address.
    """
    return "\n".join(
        [
            "[common]",
            f"server_addr = {settings.frps_server_ipv4}",  # Use IPv4
            f"server_port = {settings.frps_server_port}",
            f"user = service_{service.id}",
            f"meta_account_token = {user.account_token}",
            f"meta_service_id = {service.id}",
            "",
            f"[{service.service_name}]",
            f"type = {service.service_type.value}",
            "local_ip = 127.0.0.1",
            f"local_port = {service.local_port}",
            f"remote_port = {service.frps_internal_port}",  # Internal FRPS port
        ]
    )


def get_service_access_url(service: Service, user: User) -> tuple[str, str]:
    """Get subdomain and direct IPv6 access URLs for a service.

    Returns:
        (subdomain_url, direct_ipv6_url_with_port)
    """
    subdomain_url = f"{service.subdomain}.{settings.domain}"
    direct_url = f"[{service.sub_ipv6}]:{service.client_port}"
    return (subdomain_url, direct_url)


def generate_account_token() -> str:
    """Generate a secure random account token."""
    return secrets.token_urlsafe(24)
