import asyncio
import json
import subprocess

from fastapi import APIRouter

router = APIRouter()

NAMESPACE = "rook-ceph"


def _mgr_pod() -> str:
    result = subprocess.run(
        ["kubectl", "get", "pod", "-n", NAMESPACE, "-l", "app=rook-ceph-mgr",
         "-o", "jsonpath={.items[0].metadata.name}"],
        capture_output=True, text=True, timeout=10,
    )
    name = result.stdout.strip()
    if result.returncode != 0 or not name:
        raise RuntimeError("No rook-ceph-mgr pod found")
    return name


def _mgr_exec(*args: str) -> str:
    result = subprocess.run(
        ["kubectl", "exec", "-n", NAMESPACE, _mgr_pod(), "--", *args],
        capture_output=True, text=True, timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    return result.stdout


def _ceph(*args: str) -> dict | list:
    return json.loads(_mgr_exec("ceph", *args, "--format", "json"))


# ─── Routes ───────────────────────────────────────────────────────────────────


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
