from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy.exc import IntegrityError
from sqlmodel import Session, select

from backend.database import get_db
from backend.models import Service, ServiceStatus, ServiceType, User
from backend.schemas import (
    RegisterRequest,
    ServiceConfigResponse,
    ServiceInfo,
    ServiceResponse,
    UserDashboard,
)
from backend.utils import allocate_ipv6, generate_frpc_config, get_service_access_url

router = APIRouter(prefix="/api", tags=["services"])


@router.post("/register", response_model=ServiceResponse)
async def register_service(
    request: RegisterRequest, db: Session = Depends(get_db)
) -> ServiceResponse:
    """Register a new service tunnel."""
    try:
        user = db.exec(
            select(User).where(User.account_token == request.account_token)
        ).first()
        if not user:
            raise HTTPException(status_code=401, detail="Invalid account token")

        subdomain = f"{request.service_name}-{user.id}"

        existing_service = db.exec(
            select(Service).where(
                Service.user_id == user.id,
                Service.service_name == request.service_name,
                Service.status == ServiceStatus.active,
            )
        ).first()

        if existing_service:
            raise HTTPException(
                status_code=400,
                detail=f"Service '{request.service_name}' already exists for this account",
            )

        existing_subdomain = db.exec(
            select(Service).where(
                Service.subdomain == subdomain,
                Service.status == ServiceStatus.active,
            )
        ).first()

        if existing_subdomain:
            raise HTTPException(
                status_code=400, detail=f"Subdomain '{subdomain}' is already taken"
            )

        service_type_enum = ServiceType(request.service_type)
        ipv6_address = allocate_ipv6(db)

        service = Service(
            user_id=user.id,
            service_name=request.service_name,
            service_type=service_type_enum,
            subdomain=subdomain,
            ipv6_address=ipv6_address,
            remote_port=request.remote_port,
            local_port=request.local_port,
            status=ServiceStatus.active,
        )

        db.add(service)
        db.commit()
        db.refresh(service)

        frpc_config = generate_frpc_config(service, user)
        access_url, access_direct = get_service_access_url(service, user)

        assert service.id is not None  # type: ignore[misc]
        return ServiceResponse(
            service_id=service.id,
            service_name=request.service_name,
            service_type=request.service_type,
            subdomain=subdomain,
            ipv6_address=ipv6_address,
            remote_port=request.remote_port,
            access_url=access_url,
            access_direct=access_direct,
            frpc_config=frpc_config,
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


@router.get("/dashboard/{account_token}", response_model=UserDashboard)
async def get_dashboard(account_token: str, db: Session = Depends(get_db)):
    """Get user dashboard with all their active services."""
    user = db.exec(select(User).where(User.account_token == account_token)).first()
    if not user:
        raise HTTPException(status_code=404, detail="Account not found")

    services = db.exec(
        select(Service).where(
            Service.user_id == user.id, Service.status == ServiceStatus.active
        )
    ).all()

    services_data = []
    for service in services:
        access_url, access_direct = get_service_access_url(service, user)
        assert service.id is not None  # type: ignore[misc]
        services_data.append(
            ServiceInfo(
                service_id=service.id,
                service_name=service.service_name,
                service_type=service.service_type.value,
                subdomain=service.subdomain,
                ipv6_address=service.ipv6_address,
                remote_port=service.remote_port,
                local_port=service.local_port,
                access_url=access_url,
                access_direct=access_direct,
                created_at=service.created_at.strftime("%Y-%m-%d %H:%M:%S"),
            )
        )

    return UserDashboard(account_token=account_token, services=services_data)


@router.get("/service/{service_id}/config", response_model=ServiceConfigResponse)
async def get_service_config(service_id: int, db: Session = Depends(get_db)):
    """Get configuration details for a specific service."""
    service = db.exec(select(Service).where(Service.id == service_id)).first()
    if not service:
        raise HTTPException(status_code=404, detail="Service not found")

    user = db.exec(select(User).where(User.id == service.user_id)).first()
    if not user:
        raise HTTPException(status_code=404, detail="User not found")

    frpc_config = generate_frpc_config(service, user)
    access_url, access_direct = get_service_access_url(service, user)

    return ServiceConfigResponse(
        service_id=service_id,
        service_name=service.service_name,
        service_type=service.service_type.value,
        subdomain=service.subdomain,
        ipv6_address=service.ipv6_address,
        remote_port=service.remote_port,
        local_port=service.local_port,
        access_url=access_url,
        access_direct=access_direct,
        frpc_config=frpc_config,
    )


@router.delete("/service/{service_id}")
async def delete_service(service_id: int, db: Session = Depends(get_db)):
    """Delete (deactivate) a service."""
    service = db.exec(select(Service).where(Service.id == service_id)).first()
    if not service:
        raise HTTPException(status_code=404, detail="Service not found")

    service.status = ServiceStatus.deleted
    db.commit()

    return {"message": "Service deleted successfully", "service_id": service_id}
