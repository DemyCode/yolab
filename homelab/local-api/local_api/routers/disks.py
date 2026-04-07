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

MOUNTABLE_FSTYPES = {"ext4", "ext3", "ext2", "xfs", "btrfs", "f2fs", "ntfs", "vfat", "exfat"}


def _lsblk() -> list[dict]:
    out = subprocess.check_output(
        ["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE"],
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


def _find_storage_partition(device: dict) -> dict | None:
    for child in device.get("children") or []:
        fstype = child.get("fstype") or ""
        mp = child.get("mountpoint")
        if fstype in MOUNTABLE_FSTYPES:
            return {"name": child["name"], "mountpoint": mp}
        deeper = _find_storage_partition(child)
        if deeper:
            return deeper
    return None


def _disk_entry(device: dict) -> dict:
    mounts = _collect_mountpoints(device)
    used = 0
    for m in mounts:
        try:
            used += _df_used(m)
        except Exception:
            pass
    is_system = "/" in mounts
    storage_partition = _find_storage_partition(device)
    return {
        "name": device["name"],
        "model": (device.get("model") or "").strip(),
        "size_bytes": int(device.get("size") or 0),
        "used_bytes": used,
        "mountpoints": mounts,
        "host": settings.yolab_node_ipv6,
        "is_system": is_system,
        "storage_partition": storage_partition,
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


class EnableStorageRequest(BaseModel):
    disk_name: str


def _do_enable_storage(disk_name: str) -> str:
    """Mount the storage partition of the named disk and register it in config.toml.
    Returns the mount path. Raises ValueError on logical errors, RuntimeError on mount failure."""
    if not re.match(r"^[a-zA-Z0-9]+$", disk_name):
        raise ValueError("Invalid disk name")

    devices = _lsblk()
    disk = next((d for d in devices if d["name"] == disk_name), None)
    if not disk:
        raise ValueError("Disk not found")

    entry = _disk_entry(disk)
    partition = entry.get("storage_partition")
    if not partition:
        raise ValueError("No usable partition found on this disk")
    if partition["mountpoint"]:
        return partition["mountpoint"]  # already mounted — idempotent

    device_path = f"/dev/{partition['name']}"
    mount_path = f"/mnt/{disk_name}"

    Path(mount_path).mkdir(parents=True, exist_ok=True)
    result = subprocess.run(
        ["mount", device_path, mount_path],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())

    config_path = Path(settings.yolab_config)
    with open(config_path, "a") as f:
        f.write(f'\n[[node.mounts]]\ndevice = "{device_path}"\npath = "{mount_path}"\n')
        f.write(f'\n[[node.nfs_exports]]\npath = "{mount_path}"\n')

    return mount_path


def auto_enable_all_storage():
    """Called at startup: auto-enable storage on every non-system disk with a usable partition."""
    try:
        devices = _lsblk()
    except Exception:
        return
    for device in devices:
        if device.get("type") != "disk":
            continue
        entry = _disk_entry(device)
        if entry["is_system"]:
            continue
        partition = entry.get("storage_partition")
        if not partition or partition["mountpoint"]:
            continue
        try:
            _do_enable_storage(device["name"])
        except Exception:
            pass


@router.post("/api/disks/enable-storage")
async def enable_storage(body: EnableStorageRequest):
    try:
        mount_path = await asyncio.to_thread(_do_enable_storage, body.disk_name)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    except RuntimeError as e:
        raise HTTPException(status_code=500, detail=str(e))
    return {"ok": True, "path": mount_path}
