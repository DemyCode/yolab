import ipaddress
import secrets

from sqlmodel import Session, select

from backend.models import Service
from backend.settings import settings


def allocate_sub_ipv6(db: Session) -> str:
    used_ipv6s = set(service.sub_ipv6 for service in db.exec(select(Service)).all())
    base = ipaddress.IPv6Address(settings.ipv6_subnet_base.rstrip(":"))
    for i in range(1, 65536):
        candidate = str(base + i)
        if candidate not in used_ipv6s:
            return candidate
    raise RuntimeError("No available IPv6 addresses in subnet")


def generate_account_token() -> str:
    return secrets.token_urlsafe(24)
