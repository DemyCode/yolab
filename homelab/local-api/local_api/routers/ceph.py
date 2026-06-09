import asyncio
import json
import subprocess

from fastapi import APIRouter

from local_api import kubectl
from local_api.constants import CEPH_NAMESPACE
from local_api.models.ceph import CephStatus, OsdInfo

router = APIRouter()


def _ceph(*args: str) -> dict | list:
    return json.loads(kubectl.ceph_exec(*args, "--format", "json"))


def _cluster_status_from_k8s() -> dict:
    """Read CephCluster status from K8s CR — no exec or auth needed."""
    r = subprocess.run(
        ["kubectl", "get", "cephcluster", "-n", CEPH_NAMESPACE, "rook-ceph",
         "-o", "jsonpath={.status}"],
        capture_output=True, text=True, timeout=10,
    )
    if r.returncode != 0:
        raise RuntimeError(r.stderr.strip())
    return json.loads(r.stdout)


def _osd_counts() -> tuple[int, int]:
    """Returns (total, ready) OSD deployment counts."""
    r = subprocess.run(
        ["kubectl", "get", "deploy", "-n", CEPH_NAMESPACE, "-l", "app=rook-ceph-osd",
         "-o", "jsonpath={.items[*].status.readyReplicas}"],
        capture_output=True, text=True, timeout=10,
    )
    if r.returncode != 0:
        return 0, 0
    ready = sum(int(x) for x in r.stdout.split() if x.isdigit())
    total_r = subprocess.run(
        ["kubectl", "get", "deploy", "-n", CEPH_NAMESPACE, "-l", "app=rook-ceph-osd",
         "-o", "jsonpath={.items}"],
        capture_output=True, text=True, timeout=10,
    )
    total = len(json.loads(total_r.stdout or "[]"))
    return total, ready


@router.get("/ceph/status", response_model=CephStatus)
async def ceph_status() -> CephStatus:
    try:
        status, (osd_total, osd_ready) = await asyncio.gather(
            asyncio.to_thread(_cluster_status_from_k8s),
            asyncio.to_thread(_osd_counts),
        )
        ceph = status.get("ceph", {})
        cap = ceph.get("capacity", {})
        return CephStatus(
            available=status.get("phase") == "Ready",
            health=ceph.get("health", "HEALTH_UNKNOWN"),
            osd_count=osd_total,
            osd_up=osd_ready,
            total_bytes=cap.get("bytesTotal", 0),
            used_bytes=cap.get("bytesUsed", 0),
        )
    except Exception as e:
        return CephStatus(available=False, error=str(e))


@router.get("/ceph/osds", response_model=list[OsdInfo])
async def ceph_osds() -> list[OsdInfo]:
    try:
        data = await asyncio.to_thread(_ceph, "osd", "metadata")
        return [
            OsdInfo(
                id=osd.get("id", 0),
                hostname=osd.get("hostname", ""),
                devices=osd.get("devices", ""),
                size_bytes=int(osd.get("bluestore_bdev_size", 0)),
            )
            for osd in data
        ]
    except Exception:
        return []
