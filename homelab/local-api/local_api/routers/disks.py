import asyncio
import json
import re
import subprocess
from pathlib import Path

import httpx
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from local_api import kubectl
from local_api.settings import settings

router = APIRouter()

SYSTEM_STORAGE_PATH = "/var/yolab-data"
MOUNTABLE_FSTYPES = {"ext4", "ext3", "ext2", "xfs", "btrfs", "f2fs", "ntfs", "vfat", "exfat"}


def _lsblk() -> list[dict]:
    out = subprocess.check_output(
        ["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE"],
        text=True,
    )
    return json.loads(out)["blockdevices"]


def _find_storage_partition(device: dict) -> dict | None:
    for child in device.get("children") or []:
        if (child.get("fstype") or "") in MOUNTABLE_FSTYPES:
            return {"name": child["name"], "mountpoint": child.get("mountpoint")}
        found = _find_storage_partition(child)
        if found:
            return found
    return None


def _disk_to_entry(device: dict) -> dict:
    partition = _find_storage_partition(device)
    storage_path = None
    storage_partition = None
    if partition:
        storage_partition = partition["name"]
        storage_path = partition["mountpoint"] or f"/mnt/{device['name']}"
    return {
        "name": device["name"],
        "model": (device.get("model") or "").strip(),
        "size_bytes": int(device.get("size") or 0),
        "host": settings.yolab_node_ipv6,
        "storage_partition": storage_partition,
        "storage_path": storage_path,
    }


def _get_exported_paths() -> set[str]:
    try:
        result = subprocess.run(["exportfs", "-v"], capture_output=True, text=True)
        paths = set()
        for line in result.stdout.splitlines():
            parts = line.split()
            if parts and parts[0].startswith("/"):
                paths.add(parts[0])
        return paths
    except Exception:
        return set()


def _export_path(path: str) -> None:
    subprocess.run(
        ["exportfs", "-o", "rw,sync,no_subtree_check,no_root_squash", f"*:{path}"],
        check=True,
    )


def auto_enable_all_storage():
    """Export /var/yolab-data at startup."""
    try:
        Path(SYSTEM_STORAGE_PATH).mkdir(parents=True, exist_ok=True)
        _export_path(SYSTEM_STORAGE_PATH)
    except Exception:
        pass


def _node_ips() -> list[str]:
    ips = {settings.yolab_node_ipv6}
    try:
        nodes = kubectl.get_nodes()
        for node in nodes:
            for addr in node["status"]["addresses"]:
                if ":" in addr["address"]:
                    ips.add(addr["address"])
    except Exception:
        pass
    return list(ips)


# ---- Disk endpoints ----

@router.get("/api/disks/local")
async def disks_local():
    devices = await asyncio.to_thread(_lsblk)
    return [_disk_to_entry(d) for d in devices if d.get("type") == "disk"]


@router.get("/api/disks")
async def disks():
    ips = await asyncio.to_thread(_node_ips)
    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[client.get(f"http://[{ip}]:{settings.port}/api/disks/local") for ip in ips],
            return_exceptions=True,
        )
    return [
        disk
        for result in results
        if not isinstance(result, Exception) and result.status_code == 200
        for disk in result.json()
    ]


class EnableStorageRequest(BaseModel):
    disk_name: str
    host: str


@router.post("/api/disks/enable-storage")
async def enable_storage(body: EnableStorageRequest):
    if not re.match(r"^[a-zA-Z0-9]+$", body.disk_name):
        raise HTTPException(status_code=400, detail="Invalid disk name")

    # Forward to the right node if not local
    if body.host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=30) as client:
            r = await client.post(
                f"http://[{body.host}]:{settings.port}/api/disks/enable-storage",
                json=body.model_dump(),
            )
        if r.status_code != 200:
            raise HTTPException(status_code=r.status_code, detail=r.json().get("detail", "Failed"))
        return r.json()

    devices = await asyncio.to_thread(_lsblk)
    disk = next((d for d in devices if d["name"] == body.disk_name), None)
    if not disk:
        raise HTTPException(status_code=404, detail="Disk not found")

    entry = _disk_to_entry(disk)
    if not entry["storage_partition"]:
        raise HTTPException(status_code=400, detail="No usable partition found on this disk")

    mount_path = entry["storage_path"]
    device_path = f"/dev/{entry['storage_partition']}"
    Path(mount_path).mkdir(parents=True, exist_ok=True)

    check = await asyncio.to_thread(
        subprocess.run, ["mountpoint", "-q", mount_path], capture_output=True
    )
    if check.returncode != 0:
        result = await asyncio.to_thread(
            subprocess.run, ["mount", device_path, mount_path], capture_output=True, text=True
        )
        if result.returncode != 0:
            raise HTTPException(status_code=500, detail=result.stderr.strip())

    try:
        await asyncio.to_thread(_export_path, mount_path)
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"exportfs failed: {e}")

    return {"ok": True, "path": mount_path}


# ---- Storage endpoints (used by app install) ----

@router.get("/api/storage/local")
async def storage_local():
    exported = await asyncio.to_thread(_get_exported_paths)
    paths = exported | {SYSTEM_STORAGE_PATH}
    return [{"host": settings.yolab_node_ipv6, "path": p} for p in sorted(paths)]


@router.get("/api/storage")
async def storage_all():
    ips = await asyncio.to_thread(_node_ips)
    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[client.get(f"http://[{ip}]:{settings.port}/api/storage/local") for ip in ips],
            return_exceptions=True,
        )
    locations = []
    for ip, result in zip(ips, results):
        if isinstance(result, Exception) or result.status_code != 200:
            if ip == settings.yolab_node_ipv6:
                locations.append({"host": ip, "path": SYSTEM_STORAGE_PATH})
        else:
            locations.extend(result.json())
    return locations
