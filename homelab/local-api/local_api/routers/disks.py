import asyncio
import json
import subprocess

import httpx
from fastapi import APIRouter

from local_api import kubectl
from local_api.settings import settings

router = APIRouter()


def _lsblk() -> list[dict]:
    out = subprocess.check_output(
        ["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL"],
        text=True,
    )
    return json.loads(out)["blockdevices"]


def _df_used(mountpoint: str) -> int:
    out = subprocess.check_output(
        ["df", "-B1", "--output=used", mountpoint],
        text=True,
    )
    return int(out.strip().splitlines()[1].strip())


def _collect_mountpoints(device: dict) -> list[str]:
    mounts = []
    mp = device.get("mountpoint")
    if mp and mp != "[SWAP]":
        mounts.append(mp)
    for child in device.get("children") or []:
        mounts.extend(_collect_mountpoints(child))
    return mounts


def _disk_entry(device: dict) -> dict:
    mounts = _collect_mountpoints(device)
    used = 0
    for m in mounts:
        try:
            used += _df_used(m)
        except Exception:
            pass
    return {
        "name": device["name"],
        "model": (device.get("model") or "").strip(),
        "size_bytes": int(device.get("size") or 0),
        "used_bytes": used,
        "mountpoints": mounts,
        "host": settings.yolab_node_ipv6,
    }


@router.get("/api/disks/local")
async def disks_local():
    devices = await asyncio.to_thread(_lsblk)
    return [_disk_entry(d) for d in devices if d.get("type") == "disk"]


@router.get("/api/disks")
async def disks():
    try:
        node_ips = await asyncio.to_thread(kubectl.get_node_ips)
    except Exception:
        node_ips = []
    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[client.get(f"http://[{ip}]:{settings.port}/api/disks/local") for ip in node_ips],
            return_exceptions=True,
        )
    all_disks = []
    for r in results:
        if isinstance(r, Exception):
            continue
        if r.status_code == 200:
            all_disks.extend(r.json())
    return all_disks
