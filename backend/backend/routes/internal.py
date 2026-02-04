from datetime import datetime, timezone
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException
from sqlmodel import Session, select

from backend.database import get_db
from backend.models import Service, User
from backend.schemas import DNSResolveResponse, NFTablesRulesResponse, NFTablesRule
from backend.settings import settings

router = APIRouter(tags=["internal"])


@router.get("/services/{subdomain}", response_model=DNSResolveResponse)
async def resolve_subdomain(
    subdomain: str, db: Session = Depends(get_db)
) -> Optional[DNSResolveResponse]:
    """Resolve subdomain to IPv6 address for DNS server."""
    service = db.exec(select(Service).where(Service.subdomain == subdomain)).first()

    if service:
        return DNSResolveResponse(
            found=True, ipv6_address=service.sub_ipv6, service_id=service.id
        )
    raise HTTPException(status_code=404, detail="Service not found")


@router.get("/services", response_model=NFTablesRulesResponse)
async def get_nftables_rules(db: Session = Depends(get_db)) -> NFTablesRulesResponse:
    """Get all active services for nftables configuration.

    Returns a list of rules for the nftables-manager to apply.
    Each rule contains the IPv6 address, client port, protocol, and FRPS internal port.
    """
    services = db.exec(select(Service)).all()

    rules = []
    for service in services:
        assert service.id is not None
        rules.append(
            NFTablesRule(
                service_id=service.id,
                sub_ipv6=service.sub_ipv6,
                client_port=service.client_port,
                protocol=service.service_type.value,  # tcp or udp
                frps_internal_port=service.frps_internal_port,
            )
        )

    return NFTablesRulesResponse(rules=rules)
