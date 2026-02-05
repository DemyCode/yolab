from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy.exc import IntegrityError
from sqlmodel import Session, select
from pydantic import BaseModel

from pathlib import Path

from fastapi import APIRouter, HTTPException

from backend.schemas import (
    AvailableService,
    AvailableServicesResponse,
    ServiceTemplateResponse,
)
from backend.database import get_db
from backend.models import Service, ServiceType, User
from backend.schemas import (
    RegisterRequest,
    NFTablesRule,
    NFTablesRulesResponse,
)
from backend.utils import (
    allocate_sub_ipv6,
    allocate_frps_port,
)

router = APIRouter(tags=["services"])


@router.get("/services/{subdomain}", response_model=str)
async def resolve_subdomain(subdomain: str, db: Session = Depends(get_db)) -> str:
    service = db.exec(select(Service).where(Service.subdomain == subdomain)).first()

    if service:
        return service.sub_ipv6
    raise HTTPException(status_code=404, detail="Service not found")


@router.get("/services", response_model=NFTablesRulesResponse)
async def get_services(db: Session = Depends(get_db)) -> NFTablesRulesResponse:
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


@router.post("/services", response_model=int)
async def register_service(
    request: RegisterRequest, db: Session = Depends(get_db)
) -> int:
    try:
        user = db.exec(
            select(User).where(User.account_token == request.account_token)
        ).first()
        if not user:
            raise HTTPException(status_code=401, detail="Invalid account token")

        subdomain = f"{request.service_name}.{user.id}"

        existing_subdomain = db.exec(
            select(Service).where(
                Service.service_name == request.service_name,
                Service.user_id == user.id,
            )
        ).first()

        if existing_subdomain:
            raise HTTPException(
                status_code=400,
                detail=f"User already created this subdomain '{subdomain}' is already taken",
            )

        sub_ipv6 = allocate_sub_ipv6(db)
        frps_internal_port = allocate_frps_port(db)

        service = Service(
            user_id=user.id,
            service_name=request.service_name,
            service_type=ServiceType(request.service_type),
            sub_ipv6=sub_ipv6,
            client_port=request.client_port,
            frps_internal_port=frps_internal_port,
        )

        db.add(service)
        db.commit()
        db.refresh(service)

        assert service.id is not None

        return service.id

    except IntegrityError as e:
        db.rollback()
        raise HTTPException(
            status_code=400, detail="Registration failed due to duplicate entry"
        )
    except HTTPException:
        raise
    except Exception as e:
        db.rollback()
        raise HTTPException(
            status_code=500, detail=f"An unexpected error occurred: {str(e)}"
        )


@router.get("/services")
def list_services(db: Session = Depends(get_db)):
    services = db.exec(select(Service)).all()
    return services


class DeleteServiceRequest(BaseModel):
    service_id: int
    user_token: str


@router.delete("/services/{service_id}")
async def delete_service(
    delete_service_request: DeleteServiceRequest, db: Session = Depends(get_db)
):
    service = db.exec(
        select(Service).where(
            Service.id == delete_service_request.service_id,
            User.account_token == delete_service_request.user_token,
        )
    ).first()
    if not service:
        raise HTTPException(status_code=404, detail="Service not found")

    db.delete(service)
    db.commit()

    return {"message": "Service deleted successfully"}
