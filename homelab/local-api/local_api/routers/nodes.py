import asyncio

from fastapi import APIRouter

from local_api import kubectl

router = APIRouter()


def _parse_node(item: dict) -> dict:
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
    return {
        "name": metadata["name"],
        "ip": ip,
        "ready": ready,
        "roles": roles,
        "joined_at": metadata.get("creationTimestamp", ""),
    }


@router.get("/api/nodes")
async def nodes():
    try:
        items = await asyncio.to_thread(kubectl.get_nodes)
        return [_parse_node(item) for item in items]
    except Exception:
        return []
