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
    """Return first child partition usable for storage.
    Skips partitions mounted at system paths — only accepts unmounted ones or those under /mnt/."""
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


def _is_system_disk(device: dict) -> bool:
    """True if this disk hosts the OS — has /boot, LVM2_member, or system mountpoints."""
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


EXPORTS_FILE = Path("/etc/exports.d/yolab.exports")


def _export(path: str) -> None:
    EXPORTS_FILE.parent.mkdir(parents=True, exist_ok=True)
    line = f"{path} *(rw,sync,no_subtree_check,no_root_squash)\n"
    existing = EXPORTS_FILE.read_text() if EXPORTS_FILE.exists() else ""
    lines = [l for l in existing.splitlines(keepends=True) if not l.startswith(path + " ")]
    lines.append(line)
    EXPORTS_FILE.write_text("".join(lines))
    subprocess.run(["exportfs", "-ra"], check=True)


def _unexport(path: str) -> None:
    if EXPORTS_FILE.exists():
        lines = [l for l in EXPORTS_FILE.read_text().splitlines(keepends=True) if not l.startswith(path + " ")]
        EXPORTS_FILE.write_text("".join(lines))
    subprocess.run(["exportfs", "-ra"], check=True)


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
    devices = await asyncio.to_thread(_lsblk)
    out = []
    for d in devices:
        if d.get("type") != "disk":
            continue
        partition = _find_storage_partition(d)
        if partition:
            storage_path = partition["mountpoint"] or f"/mnt/{d['name']}"
        elif _is_system_disk(d):
            storage_path = SYSTEM_STORAGE_PATH
        else:
            storage_path = None
        out.append(
            {
                "name": d["name"],
                "model": (d.get("model") or "").strip(),
                "size_bytes": int(d.get("size") or 0),
                "host": settings.yolab_node_ipv6,
                "storage_partition": partition["name"] if partition else None,
                "storage_path": storage_path,
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

    if partition:
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
    elif _is_system_disk(disk):
        mount_path = SYSTEM_STORAGE_PATH
        Path(mount_path).mkdir(parents=True, exist_ok=True)
    else:
        raise HTTPException(
            status_code=400, detail="No usable partition found on this disk"
        )

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
            raise HTTPException(
                status_code=r.status_code, detail=r.json().get("detail", "Failed")
            )
        return r.json()

    await asyncio.to_thread(_unexport, body.path)
    return {"ok": True}
