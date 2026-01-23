from pathlib import Path

from fastapi import APIRouter, HTTPException

from backend.schemas import (
    AvailableService,
    AvailableServicesResponse,
    ServiceTemplateResponse,
)

router = APIRouter(prefix="/api/services", tags=["templates"])


@router.get("", response_model=AvailableServicesResponse)
async def list_available_services():
    """List all available service templates."""
    services_dir = Path(__file__).parent.parent.parent / "services"

    if not services_dir.exists():
        return AvailableServicesResponse(services=[])

    services = []
    for service_path in services_dir.iterdir():
        if service_path.is_dir():
            docker_compose_path = service_path / "docker-compose.yml"
            caddyfile_path = service_path / "Caddyfile"

            services.append(
                AvailableService(
                    name=service_path.name,
                    has_docker_compose=docker_compose_path.exists(),
                    has_caddyfile=caddyfile_path.exists(),
                )
            )

    return AvailableServicesResponse(services=services)


@router.get("/{service_name}", response_model=ServiceTemplateResponse)
async def get_service_template(service_name: str):
    """Get all configuration files for a specific service template."""
    services_dir = Path(__file__).parent.parent.parent / "services"
    service_dir = services_dir / service_name

    if not service_dir.exists() or not service_dir.is_dir():
        raise HTTPException(
            status_code=404, detail=f"Service '{service_name}' not found"
        )

    docker_compose_path = service_dir / "docker-compose.yml"
    caddyfile_path = service_dir / "Caddyfile"

    docker_compose_content = None
    if docker_compose_path.exists():
        docker_compose_content = docker_compose_path.read_text()

    caddyfile_content = None
    if caddyfile_path.exists():
        caddyfile_content = caddyfile_path.read_text()

    return ServiceTemplateResponse(
        service_name=service_name,
        docker_compose=docker_compose_content,
        caddyfile=caddyfile_content,
    )
