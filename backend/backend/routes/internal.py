from datetime import datetime, timezone

from fastapi import APIRouter, Depends, HTTPException
from sqlmodel import Session, select

from backend.database import get_db
from backend.models import Service, ServiceStatus, User
from backend.schemas import (
    AuthValidateRequest,
    AuthValidateResponse,
    DNSResolveResponse,
    LastSeenResponse,
)
from backend.settings import settings

router = APIRouter(prefix="/internal", tags=["internal"])


@router.post("/auth/validate", response_model=AuthValidateResponse)
async def validate_auth(
    request: AuthValidateRequest, db: Session = Depends(get_db)
) -> AuthValidateResponse:
    """Validate authentication token for auth plugin."""
    user = db.exec(
        select(User).where(User.account_token == request.account_token)
    ).first()

    if not user:
        return AuthValidateResponse(valid=False, reason="Invalid account token")

    service = db.exec(
        select(Service).where(
            Service.id == request.service_id, Service.user_id == user.id
        )
    ).first()

    if not service:
        return AuthValidateResponse(
            valid=False, reason="Service not found or does not belong to this account"
        )

    if service.status != ServiceStatus.active:
        return AuthValidateResponse(
            valid=False, reason=f"Service is {service.status.value}"
        )

    if service.service_type.value != request.proxy_type:
        return AuthValidateResponse(
            valid=False,
            reason=f"Service is for {service.service_type.value}, not {request.proxy_type}",
        )

    if request.remote_ip != service.ipv6_address:
        return AuthValidateResponse(
            valid=False,
            reason=f"IPv6 mismatch: expected {service.ipv6_address}, got {request.remote_ip}",
        )

    if request.remote_port != service.remote_port:
        return AuthValidateResponse(
            valid=False,
            reason=f"Port mismatch: expected {service.remote_port}, got {request.remote_port}",
        )

    return AuthValidateResponse(valid=True, service_id=service.id)


@router.post("/service/{service_id}/last-seen", response_model=LastSeenResponse)
async def update_last_seen(service_id: int, db: Session = Depends(get_db)):
    """Update last_seen timestamp for a service."""
    service = db.exec(select(Service).where(Service.id == service_id)).first()
    if not service:
        raise HTTPException(status_code=404, detail="Service not found")

    service.last_seen = datetime.now(timezone.utc)
    db.add(service)
    db.commit()

    return LastSeenResponse(success=True, service_id=service_id)


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
