import asyncio
import json

from fastapi import APIRouter

from local_api import kubectl

router = APIRouter()

def _ceph(*args: str) -> dict | list:
    return json.loads(kubectl.ceph_exec(*args, "--format", "json"))


@router.get("/ceph/status")
async def ceph_status():
    try:
        data = await asyncio.to_thread(_ceph, "status")
        pg = data.get("pgmap", {})
        osd = data.get("osdmap", {})
        return {
            "available": True,
            "health": data.get("health", {}).get("status", "HEALTH_UNKNOWN"),
            "osd_count": osd.get("num_osds", 0),
            "osd_up": osd.get("num_up_osds", 0),
            "total_bytes": pg.get("bytes_total", 0),
            "used_bytes": pg.get("bytes_used", 0),
        }
    except Exception as e:
        return {"available": False, "error": str(e)}


@router.get("/ceph/osds")
async def ceph_osds():
    try:
        data = await asyncio.to_thread(_ceph, "osd", "metadata")
        return [
            {
                "id": osd.get("id"),
                "hostname": osd.get("hostname"),
                "devices": osd.get("devices", ""),
                "size_bytes": int(osd.get("bluestore_bdev_size", 0)),
            }
            for osd in data
        ]
    except Exception:
        return []
