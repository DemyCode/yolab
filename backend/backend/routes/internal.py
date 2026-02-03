from datetime import datetime, timezone

from fastapi import APIRouter, Depends, HTTPException
from sqlmodel import Session, select

from backend.database import get_db
from backend.models import Service, ServiceStatus, User
from backend.schemas import DNSResolveResponse
from backend.settings import settings

router = APIRouter(tags=["internal"])


@router.get("/dns/resolve/{subdomain}", response_model=DNSResolveResponse)
async def resolve_subdomain(
    subdomain: str, db: Session = Depends(get_db)
) -> DNSResolveResponse:
    """Resolve subdomain to IPv6 address for DNS server."""
    service = db.exec(
        select(Service).where(
            Service.subdomain == subdomain, Service.status == ServiceStatus.active
        )
    ).first()

    if service:
        return DNSResolveResponse(
            found=True, ipv6_address=service.ipv6_address, service_id=service.id
        )

    # Return main server IPv6 as fallback
    return DNSResolveResponse(
        found=False, ipv6_address=settings.frps_server_ipv6, fallback_to_main=True
    )
