from fastapi import APIRouter, Depends
from sqlmodel import Session, select

from backend.database import get_db
from backend.models import Service, ServiceStatus, ServiceType, User
from backend.schemas import StatsResponse
from backend.settings import settings

router = APIRouter(prefix="/api", tags=["stats"])


@router.get("/stats", response_model=StatsResponse)
async def get_stats(db: Session = Depends(get_db)):
    """Get platform statistics."""
    total_users = len(db.exec(select(User)).all())
    active_services = db.exec(
        select(Service).where(Service.status == ServiceStatus.active)
    ).all()

    tcp_count = sum(1 for s in active_services if s.service_type == ServiceType.tcp)
    udp_count = sum(1 for s in active_services if s.service_type == ServiceType.udp)

    return StatsResponse(
        total_users=total_users,
        total_services=len(active_services),
        tcp_services=tcp_count,
        udp_services=udp_count,
        ipv6_subnet=settings.ipv6_subnet_base,
    )
