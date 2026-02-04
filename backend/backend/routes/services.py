from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy.exc import IntegrityError
from sqlmodel import Session, select
from pydantic import BaseModel

from backend.database import get_db
from backend.models import Service, ServiceType, User
from backend.schemas import (
    RegisterRequest,
    ServiceResponse,
)
from backend.utils import (
    allocate_sub_ipv6,
    allocate_frps_port,
)

router = APIRouter(tags=["services"])


@router.post("/services", response_model=ServiceResponse)
async def register_service(
    request: RegisterRequest, db: Session = Depends(get_db)
) -> ServiceResponse:
    try:
        user = db.exec(
            select(User).where(User.account_token == request.account_token)
        ).first()
        if not user:
            raise HTTPException(status_code=401, detail="Invalid account token")

        subdomain = f"{request.service_name}.{user.id}"

        existing_subdomain = db.exec(
            select(Service).where(
                Service.subdomain == subdomain,
            )
        ).first()

        if existing_subdomain:
            raise HTTPException(
                status_code=400, detail=f"Subdomain '{subdomain}' is already taken"
            )

        service_type_enum = ServiceType(request.service_type)

        sub_ipv6 = allocate_sub_ipv6(db)
        frps_internal_port = allocate_frps_port(db)

        service = Service(
            user_id=user.id,
            service_name=request.service_name,
            service_type=service_type_enum,
            subdomain=subdomain,
            sub_ipv6=sub_ipv6,
            client_port=request.client_port,
            frps_internal_port=frps_internal_port,
            local_port=request.local_port,
        )

        db.add(service)
        db.commit()
        db.refresh(service)

        assert service.id is not None  # type: ignore[misc]
        return ServiceResponse(
            service_id=service.id,
            subdomain=subdomain,
            sub_ipv6=sub_ipv6,
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
