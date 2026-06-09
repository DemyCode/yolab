import asyncio
import tomllib
from pathlib import Path

from fastapi import APIRouter, HTTPException

from local_api import kubectl
from local_api.models.nodes import JoinInfo, NodeInfo
from local_api.settings import settings

router = APIRouter()


def _parse_node(item: dict) -> NodeInfo:
    metadata = item["metadata"]
    labels = metadata.get("labels", {})
    roles = [
        key.removeprefix("node-role.kubernetes.io/")
        for key in labels
        if key.startswith("node-role.kubernetes.io/")
    ]
    ip = ""
    for addr in item["status"].get("addresses", []):
        if addr["type"] == "InternalIP":
            ip = addr["address"]
            break
    ready = any(
        c["type"] == "Ready" and c["status"] == "True"
        for c in item["status"].get("conditions", [])
    )
    return NodeInfo(
        name=metadata["name"],
        ip=ip,
        ready=ready,
        roles=roles,
        joined_at=metadata.get("creationTimestamp", ""),
    )


@router.get("/nodes", response_model=list[NodeInfo])
async def nodes() -> list[NodeInfo]:
    try:
        items = await asyncio.to_thread(kubectl.get_nodes)
        return [_parse_node(item) for item in items]
    except Exception:
        return []


@router.get("/cluster/join-info", response_model=JoinInfo)
async def cluster_join_info() -> JoinInfo:
    try:
        cfg = tomllib.loads(Path(settings.yolab_config).read_text())
        k3s_token = cfg["node"]["k3s"]["token"]
        sub_ipv6_private = cfg["tunnel"]["sub_ipv6_private"]
        return JoinInfo(
            k3s_token=k3s_token,
            server_addr=f"https://[{sub_ipv6_private}]:6443",
        )
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))
