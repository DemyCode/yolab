import asyncio
import json
import re
import subprocess
import tomllib
from pathlib import Path

import httpx
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

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


def _collect_partitions(device: dict) -> list[dict]:
    parts = []
    for child in device.get("children") or []:
        if child.get("type") == "part":
            parts.append({
                "name": child["name"],
                "size_bytes": int(child.get("size") or 0),
                "mountpoint": child.get("mountpoint"),
            })
        parts.extend(_collect_partitions(child))
    return parts


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
        "partitions": _collect_partitions(device),
        "host": settings.yolab_node_ipv6,
    }


@router.get("/api/disks/local")
async def disks_local():
    devices = await asyncio.to_thread(_lsblk)
    return [_disk_entry(d) for d in devices if d.get("type") == "disk"]


@router.get("/api/disks")
async def disks():
    try:
        nodes = await asyncio.to_thread(kubectl.get_nodes)
        ip_to_name = {
            addr["address"]: node["metadata"]["name"]
            for node in nodes
            for addr in node["status"]["addresses"]
        }
        node_ips = [ip for ip in ip_to_name if ":" in ip]
    except Exception:
        ip_to_name = {}
        node_ips = []

    seen_hosts = set()
    all_disks = []

    local_disks = await asyncio.to_thread(_lsblk)
    local_node_name = ip_to_name.get(settings.yolab_node_ipv6, settings.yolab_node_ipv6)
    for d in local_disks:
        if d.get("type") == "disk":
            entry = _disk_entry(d)
            entry["node_name"] = local_node_name
            all_disks.append(entry)
    seen_hosts.add(settings.yolab_node_ipv6)

    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[
                client.get(f"http://[{ip}]:{settings.port}/api/disks/local")
                for ip in node_ips
                if ip not in seen_hosts
            ],
            return_exceptions=True,
        )
    for r in results:
        if isinstance(r, Exception):
            continue
        if r.status_code == 200:
            for disk in r.json():
                disk["node_name"] = ip_to_name.get(disk["host"], disk["host"])
                all_disks.append(disk)
    return all_disks


class MountRequest(BaseModel):
    device: str
    path: str


@router.post("/api/disks/mount")
async def mount_disk(body: MountRequest):
    if not re.match(r"^/dev/[a-zA-Z0-9]+$", body.device):
        raise HTTPException(status_code=400, detail="Invalid device path")
    if not re.match(r"^/[a-zA-Z0-9/_-]+$", body.path):
        raise HTTPException(status_code=400, detail="Invalid mount path")

    Path(body.path).mkdir(parents=True, exist_ok=True)

    result = await asyncio.to_thread(
        subprocess.run,
        ["mount", body.device, body.path],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise HTTPException(status_code=500, detail=result.stderr.strip())

    config_path = Path(settings.yolab_config)
    with open(config_path, "a") as f:
        f.write(f'\n[[node.mounts]]\ndevice = "{body.device}"\npath = "{body.path}"\n')
        f.write(f'\n[[node.nfs_exports]]\npath = "{body.path}"\n')

    return {"ok": True, "path": body.path}
