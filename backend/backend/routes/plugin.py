from datetime import datetime, timezone

from fastapi import APIRouter, Depends, Query
from sqlmodel import Session, select

from backend.database import get_db
from backend.models import Service, ServiceStatus, User
from backend.schemas import PluginContent, PluginResponse

router = APIRouter(tags=["plugin"])


@router.post("/handler")
async def handle_plugin_request(
    content: PluginContent,
    version: str = Query(...),
    op: str = Query(...),
    db: Session = Depends(get_db),
) -> PluginResponse:
    from devtools import pprint

    pprint({"version": version, "op": op, "content": content})
    if op != "NewProxy":
        return PluginResponse(reject=False, reject_reason="", unchange=True)

    account_token = content.user.metas.get("account_token", "")
    service_id_str = content.user.metas.get("service_id", "")

    if not account_token:
        return PluginResponse(
            reject=True,
            reject_reason="Missing account_token in metadata",
            unchange=True,
        )

    if not service_id_str:
        return PluginResponse(
            reject=True, reject_reason="Missing service_id in metadata", unchange=True
        )

    try:
        service_id = int(service_id_str)
    except ValueError:
        return PluginResponse(
            reject=True, reject_reason="Invalid service_id format", unchange=True
        )

    user = db.exec(select(User).where(User.account_token == account_token)).first()

    if not user:
        return PluginResponse(
            reject=True, reject_reason="Invalid account token", unchange=True
        )

    service = db.exec(
        select(Service).where(Service.id == service_id, Service.user_id == user.id)
    ).first()

    if not service:
        return PluginResponse(
            reject=True,
            reject_reason="Service not found or does not belong to this account",
            unchange=True,
        )

    if service.status != ServiceStatus.active:
        return PluginResponse(
            reject=True,
            reject_reason=f"Service is {service.status.value}",
            unchange=True,
        )

    if service.service_type.value != content.proxy_type:
        return PluginResponse(
            reject=True,
            reject_reason=f"Service is for {service.service_type.value}, not {content.proxy_type}",
            unchange=True,
        )

    if content.remote_ip != service.ipv6_address:
        return PluginResponse(
            reject=True,
            reject_reason=f"IPv6 mismatch: expected {service.ipv6_address}, got {content.remote_ip}",
            unchange=True,
        )

    if content.remote_port != service.remote_port:
        return PluginResponse(
            reject=True,
            reject_reason=f"Port mismatch: expected {service.remote_port}, got {content.remote_port}",
            unchange=True,
        )

    # Update last_seen timestamp
    service.last_seen = datetime.now(timezone.utc)
    db.add(service)
    db.commit()

    return PluginResponse(reject=False, reject_reason="", unchange=True)
