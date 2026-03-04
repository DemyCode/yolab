from typing import List

from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy.exc import IntegrityError
from sqlmodel import Session, select

from backend.database import get_db
from backend.models import Service, User
from backend.schemas import (
    DNSResolveResponse,
    RegisterRequest,
    RegisterResponse,
    WireGuardPeer,
)
from backend.settings import settings
from backend.utils import allocate_sub_ipv6

router = APIRouter(tags=["services"])


@router.get("/dns/resolve/{service_name}", response_model=DNSResolveResponse)
async def dns_resolve(
    service_name: str, db: Session = Depends(get_db)
) -> DNSResolveResponse:
    service = db.exec(
        select(Service).where(Service.service_name == service_name)
    ).first()
    if service:
        return DNSResolveResponse(found=True, ipv6_address=service.sub_ipv6)
    return DNSResolveResponse(found=False)


@router.get("/wireguard/peers", response_model=List[WireGuardPeer])
async def get_wireguard_peers(db: Session = Depends(get_db)) -> List[WireGuardPeer]:
    services = db.exec(select(Service)).all()
    return [
        WireGuardPeer(sub_ipv6=s.sub_ipv6, wg_public_key=s.wg_public_key)
        for s in services
    ]


@router.post("/services", response_model=RegisterResponse)
async def register_service(
    request: RegisterRequest, db: Session = Depends(get_db)
) -> RegisterResponse:
    try:
        user = db.exec(
            select(User).where(User.account_token == request.account_token)
        ).first()
        if not user:
            raise HTTPException(status_code=401, detail="Invalid account token")

        existing = db.exec(
            select(Service).where(
                Service.service_name == request.service_name,
                Service.user_id == user.id,
            )
        ).first()
        if existing:
            raise HTTPException(
                status_code=400,
                detail=f"Service '{request.service_name}' already exists",
            )

        sub_ipv6 = allocate_sub_ipv6(db)

        service = Service(
            user_id=user.id,
            service_name=request.service_name,
            sub_ipv6=sub_ipv6,
            wg_public_key=request.wg_public_key,
        )

        db.add(service)
        db.commit()
        db.refresh(service)

        assert service.id is not None

        return RegisterResponse(
            service_id=service.id,
            sub_ipv6=sub_ipv6,
            wg_server_endpoint=settings.wg_server_endpoint,
            wg_server_public_key=settings.wg_server_public_key,
        )

    except IntegrityError:
        db.rollback()
        raise HTTPException(
            status_code=400, detail="Registration failed due to duplicate entry"
        )
    except HTTPException:
        raise
    except Exception as e:
        db.rollback()
        raise HTTPException(status_code=500, detail=f"Unexpected error: {str(e)}")


@router.delete("/services/{service_id}")
async def delete_service(
    service_id: int, user_token: str, db: Session = Depends(get_db)
):
    user = db.exec(select(User).where(User.account_token == user_token)).first()
    if not user:
        raise HTTPException(status_code=401, detail="Invalid account token")

    service = db.exec(
        select(Service).where(Service.id == service_id, Service.user_id == user.id)
    ).first()
    if not service:
        raise HTTPException(status_code=404, detail="Service not found")

    db.delete(service)
    db.commit()
    return {"message": "Service deleted"}
