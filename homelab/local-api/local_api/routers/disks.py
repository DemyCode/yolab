import asyncio
import json
import subprocess
from typing import Literal

import httpx
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from local_api import kubectl
from local_api.settings import settings

router = APIRouter()

DiskStatus = Literal["osd", "pending_osd", "needs_format", "system"]

CEPH_NAMESPACE = "rook-ceph"


def _node_ips() -> list[str]:
    ips = {settings.yolab_node_ipv6}
    try:
        for node in kubectl.get_nodes():
            for addr in node["status"]["addresses"]:
                if ":" in addr["address"]:
                    ips.add(addr["address"])
    except Exception:
        pass
    return list(ips)


def _lsblk() -> list[dict]:
    out = subprocess.check_output(
        ["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE"],
        text=True,
    )
    return json.loads(out)["blockdevices"]


def _is_system_disk(device: dict) -> bool:
    for child in device.get("children") or []:
        mp = child.get("mountpoint") or ""
        fstype = child.get("fstype") or ""
        if mp.startswith("/boot") or mp == "/" or mp.startswith("/nix"):
            return True
        if fstype == "LVM2_member":
            return True
        if _is_system_disk(child):
            return True
    return False


def _has_content(device: dict) -> bool:
    """True if the disk has partitions or a filesystem Rook won't auto-claim."""
    if device.get("fstype"):
        return True
    return bool(device.get("children"))


def _first_fstype(device: dict) -> str | None:
    if device.get("fstype"):
        return device["fstype"]
    for child in device.get("children") or []:
        fstype = child.get("fstype")
        if fstype:
            return fstype
    return None


def _ceph_osd_map() -> dict[str, int]:
    """Returns {device_name: osd_id} from Ceph OSD metadata."""
    try:
        result = subprocess.run(
            [
                "kubectl", "exec", "-n", CEPH_NAMESPACE,
                "-l", "app=rook-ceph-mgr", "--",
                "ceph", "osd", "metadata", "--format", "json",
            ],
            capture_output=True, text=True, timeout=15,
        )
        if result.returncode != 0:
            return {}
        data = json.loads(result.stdout)
        mapping: dict[str, int] = {}
        for osd in data:
            osd_id = osd.get("id")
            if osd_id is None:
                continue
            for dev in osd.get("devices", "").split(","):
                dev = dev.strip().replace("/dev/", "")
                if dev:
                    mapping[dev] = int(osd_id)
        return mapping
    except Exception:
        return {}


async def _gather_from_nodes(path: str) -> list[tuple[str, list]]:
    ips = await asyncio.to_thread(_node_ips)
    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[client.get(f"http://[{ip}]:{settings.port}{path}") for ip in ips],
            return_exceptions=True,
        )
    return [
        (ip, r.json())
        for ip, r in zip(ips, results)
        if isinstance(r, httpx.Response) and r.status_code == 200
    ]


@router.get("/disks/local")
async def disks_local():
    devices, osd_map = await asyncio.gather(
        asyncio.to_thread(_lsblk),
        asyncio.to_thread(_ceph_osd_map),
    )
    out = []
    for d in devices:
        if d.get("type") != "disk":
            continue
        name = d["name"]
        if _is_system_disk(d):
            status: DiskStatus = "system"
            osd_id = None
        elif name in osd_map:
            status = "osd"
            osd_id = osd_map[name]
        elif _has_content(d):
            status = "needs_format"
            osd_id = None
        else:
            status = "pending_osd"
            osd_id = None

        out.append({
            "name": name,
            "model": (d.get("model") or "").strip(),
            "size_bytes": int(d.get("size") or 0),
            "host": settings.yolab_node_ipv6,
            "status": status,
            "ceph_osd_id": osd_id,
            "fs_type": _first_fstype(d),
        })
    return out


@router.get("/disks")
async def disks():
    return [
        disk
        for _, disks in await _gather_from_nodes("/api/disks/local")
        for disk in disks
    ]


class FormatRequest(BaseModel):
    disk_name: str
    host: str


@router.post("/disks/format")
async def format_disk(body: FormatRequest):
    if body.host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=60) as client:
            r = await client.post(
                f"http://[{body.host}]:{settings.port}/api/disks/format",
                json=body.model_dump(),
            )
        if r.status_code != 200:
            raise HTTPException(r.status_code, r.json().get("detail", "Failed"))
        return r.json()

    devices = await asyncio.to_thread(_lsblk)
    disk = next((d for d in devices if d["name"] == body.disk_name), None)
    if not disk:
        raise HTTPException(404, "Disk not found")
    if _is_system_disk(disk):
        raise HTTPException(400, "Cannot format system disk")

    r1 = await asyncio.to_thread(
        subprocess.run,
        ["wipefs", "--all", "--force", f"/dev/{body.disk_name}"],
        capture_output=True, text=True,
    )
    r2 = await asyncio.to_thread(
        subprocess.run,
        ["sgdisk", "--zap-all", f"/dev/{body.disk_name}"],
        capture_output=True, text=True,
    )
    if r1.returncode != 0:
        raise HTTPException(500, f"wipefs failed: {r1.stderr.strip()}")
    if r2.returncode != 0:
        raise HTTPException(500, f"sgdisk failed: {r2.stderr.strip()}")

    return {"ok": True}
