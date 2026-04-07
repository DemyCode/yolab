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
MOUNTABLE_FSTYPES = {
    "ext4",
    "ext3",
    "ext2",
    "xfs",
    "btrfs",
    "f2fs",
    "ntfs",
    "vfat",
    "exfat",
}


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


def _find_storage_partition(device: dict) -> dict | None:
    """Return first child partition that is usable for storage.
    Skips partitions mounted at system paths (/, /boot, /nix, etc.).
    Only accepts unmounted partitions or ones already under /mnt/."""
    for child in device.get("children") or []:
        fstype = child.get("fstype") or ""
        mountpoint = child.get("mountpoint")
        if fstype in MOUNTABLE_FSTYPES:
            if mountpoint is None or mountpoint.startswith("/mnt/"):
                return {"name": child["name"], "mountpoint": mountpoint}
        found = _find_storage_partition(child)
        if found:
            return found
    return None


def _exported_paths() -> list[str]:
    try:
        out = subprocess.run(["exportfs", "-v"], capture_output=True, text=True).stdout
        return [
            line.split()[0]
            for line in out.splitlines()
            if line.split() and line.split()[0].startswith("/")
        ]
    except Exception:
        return []


def _export(path: str) -> None:
    subprocess.run(
        ["exportfs", "-o", "rw,sync,no_subtree_check,no_root_squash", f"*:{path}"],
        check=True,
    )


def auto_enable_all_storage():
    """Export /var/yolab-data at startup so it is always available."""
    try:
        Path(SYSTEM_STORAGE_PATH).mkdir(parents=True, exist_ok=True)
        _export(SYSTEM_STORAGE_PATH)
    except Exception:
        pass


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
        if not isinstance(r, Exception) and r.status_code == 200
    ]


@router.get("/api/disks/local")
async def disks_local():
    """All physical disks on this node."""
    devices = await asyncio.to_thread(_lsblk)
    out = []
    for d in devices:
        if d.get("type") != "disk":
            continue
        partition = _find_storage_partition(d)
        out.append(
            {
                "name": d["name"],
                "model": (d.get("model") or "").strip(),
                "size_bytes": int(d.get("size") or 0),
                "host": settings.yolab_node_ipv6,
                "storage_partition": partition["name"] if partition else None,
                "storage_path": partition["mountpoint"] or f"/mnt/{d['name']}"
                if partition
                else None,
            }
        )
    return out


@router.get("/api/disks")
async def disks():
    return [
        disk
        for _, disks in await _gather_from_nodes("/api/disks/local")
        for disk in disks
    ]


@router.get("/api/storage/local")
async def storage_local():
    paths = await asyncio.to_thread(_exported_paths)
    return [{"host": settings.yolab_node_ipv6, "path": p} for p in paths]


@router.get("/api/storage")
async def storage():
    """All NFS-exported storage locations across the swarm."""
    return [
        entry
        for _ip, entries in await _gather_from_nodes("/api/storage/local")
        for entry in entries
    ]


class EnableStorageRequest(BaseModel):
    disk_name: str
    host: str


@router.post("/api/disks/enable-storage")
async def enable_storage(body: EnableStorageRequest):
    if body.host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=30) as client:
            r = await client.post(
                f"http://[{body.host}]:{settings.port}/api/disks/enable-storage",
                json=body.model_dump(),
            )
        if r.status_code != 200:
            raise HTTPException(
                status_code=r.status_code, detail=r.json().get("detail", "Failed")
            )
        return r.json()

    devices = await asyncio.to_thread(_lsblk)
    disk = next((d for d in devices if d["name"] == body.disk_name), None)
    if not disk:
        raise HTTPException(status_code=404, detail="Disk not found")

    partition = _find_storage_partition(disk)
    if not partition:
        raise HTTPException(
            status_code=400, detail="No usable partition found on this disk"
        )

    mount_path = partition["mountpoint"] or f"/mnt/{body.disk_name}"
    Path(mount_path).mkdir(parents=True, exist_ok=True)

    if not partition["mountpoint"]:
        result = await asyncio.to_thread(
            subprocess.run,
            ["mount", f"/dev/{partition['name']}", mount_path],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            raise HTTPException(status_code=500, detail=result.stderr.strip())

    try:
        await asyncio.to_thread(_export, mount_path)
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"exportfs failed: {e}")

    return {"ok": True, "path": mount_path}


class DisableStorageRequest(BaseModel):
    path: str
    host: str


@router.post("/api/disks/disable-storage")
async def disable_storage(body: DisableStorageRequest):
    if body.host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=30) as client:
            r = await client.post(
                f"http://[{body.host}]:{settings.port}/api/disks/disable-storage",
                json=body.model_dump(),
            )
        if r.status_code != 200:
            raise HTTPException(status_code=r.status_code, detail=r.json().get("detail", "Failed"))
        return r.json()

    subprocess.run(["exportfs", "-u", f"*:{body.path}"], capture_output=True)
    return {"ok": True}
