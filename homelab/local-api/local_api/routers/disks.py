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

SYSTEM_STORAGE_PATH = "/var/yolab-data"


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


def _get_exported_paths() -> set[str]:
    result = subprocess.run(["exportfs", "-v"], capture_output=True, text=True)
    paths = set()
    for line in result.stdout.splitlines():
        parts = line.split()
        if parts and parts[0].startswith("/"):
            paths.add(parts[0])
    return paths


def _export_path(path: str) -> None:
    subprocess.run(
        ["exportfs", "-o", "rw,sync,no_subtree_check,no_root_squash", f"*:{path}"],
        check=True,
    )


def _unexport_path(path: str) -> None:
    subprocess.run(["exportfs", "-u", f"*:{path}"], capture_output=True)


def _disk_entry(device: dict, exported_paths: set[str]) -> dict:
    mounts = _collect_mountpoints(device)
    used = 0
    for m in mounts:
        try:
            used += _df_used(m)
        except Exception:
            pass
    is_system = "/" in mounts
    storage_partition = _find_storage_partition(device)

    if is_system:
        storage_path = SYSTEM_STORAGE_PATH
    elif storage_partition and storage_partition["mountpoint"]:
        storage_path = storage_partition["mountpoint"]
    else:
        storage_path = None

    storage_enabled = is_system or (storage_path is not None and storage_path in exported_paths)

    return {
        "name": device["name"],
        "model": (device.get("model") or "").strip(),
        "size_bytes": int(device.get("size") or 0),
        "used_bytes": used,
        "mountpoints": mounts,
        "host": settings.yolab_node_ipv6,
        "is_system": is_system,
        "storage_partition": storage_partition,
        "storage_path": storage_path,
        "storage_enabled": storage_enabled,
    }


@router.get("/api/disks/local")
async def disks_local():
    devices = await asyncio.to_thread(_lsblk)
    exported = await asyncio.to_thread(_get_exported_paths)
    return [_disk_entry(d, exported) for d in devices if d.get("type") == "disk"]


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

    local_devices, exported = await asyncio.gather(
        asyncio.to_thread(_lsblk),
        asyncio.to_thread(_get_exported_paths),
    )
    local_node_name = ip_to_name.get(settings.yolab_node_ipv6, settings.yolab_node_ipv6)
    for d in local_devices:
        if d.get("type") == "disk":
            entry = _disk_entry(d, exported)
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
    if not re.match(r"^[a-zA-Z0-9]+$", disk_name):
        raise ValueError("Invalid disk name")

    devices = _lsblk()
    disk = next((d for d in devices if d["name"] == disk_name), None)
    if not disk:
        raise ValueError("Disk not found")

    entry = _disk_entry(disk, set())
    if entry["is_system"]:
        # System disk: ensure /var/yolab-data is exported
        Path(SYSTEM_STORAGE_PATH).mkdir(parents=True, exist_ok=True)
        _export_path(SYSTEM_STORAGE_PATH)
        return SYSTEM_STORAGE_PATH

    partition = entry.get("storage_partition")
    if not partition:
        raise ValueError("No usable partition found on this disk")

    mount_path = partition["mountpoint"] or f"/mnt/{disk_name}"

    if not partition["mountpoint"]:
        device_path = f"/dev/{partition['name']}"
        Path(mount_path).mkdir(parents=True, exist_ok=True)
        result = subprocess.run(
            ["mount", device_path, mount_path],
            capture_output=True, text=True,
        )
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip())

    _export_path(mount_path)
    return mount_path


def _do_disable_storage(disk_name: str) -> None:
    if not re.match(r"^[a-zA-Z0-9]+$", disk_name):
        raise ValueError("Invalid disk name")

    devices = _lsblk()
    disk = next((d for d in devices if d["name"] == disk_name), None)
    if not disk:
        raise ValueError("Disk not found")

    entry = _disk_entry(disk, set())
    if entry["is_system"]:
        raise ValueError("Cannot disable system disk")

    storage_path = entry.get("storage_path")
    if storage_path:
        _unexport_path(storage_path)

    partition = entry.get("storage_partition")
    if partition and partition["mountpoint"]:
        subprocess.run(["umount", partition["mountpoint"]], capture_output=True)


def _ensure_system_storage_exported() -> None:
    Path(SYSTEM_STORAGE_PATH).mkdir(parents=True, exist_ok=True)
    _export_path(SYSTEM_STORAGE_PATH)


def auto_enable_all_storage():
    """Called at startup: export system storage and re-export any already-mounted non-system disks."""
    try:
        _ensure_system_storage_exported()
    except Exception:
        pass
    try:
        devices = _lsblk()
    except Exception:
        return
    for device in devices:
        if device.get("type") != "disk":
            continue
        entry = _disk_entry(device, set())
        if entry["is_system"]:
            continue
        partition = entry.get("storage_partition")
        if partition and partition["mountpoint"]:
            try:
                _export_path(partition["mountpoint"])
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


@router.get("/api/storage/local")
async def storage_local():
    """Returns NFS-exported storage paths on this node (always includes system storage)."""
    exported = await asyncio.to_thread(_get_exported_paths)
    # Always include system storage path regardless of export state
    paths = {SYSTEM_STORAGE_PATH} | exported
    return [{"host": settings.yolab_node_ipv6, "path": p} for p in sorted(paths)]


@router.get("/api/storage")
async def storage_all():
    """Returns all available storage locations across the cluster."""
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

    local_name = ip_to_name.get(settings.yolab_node_ipv6, settings.yolab_node_ipv6)
    exported = await asyncio.to_thread(_get_exported_paths)
    paths = {SYSTEM_STORAGE_PATH} | exported
    locations = [
        {"host": settings.yolab_node_ipv6, "node_name": local_name, "path": p}
        for p in sorted(paths)
    ]
    seen_hosts = {settings.yolab_node_ipv6}

    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[
                client.get(f"http://[{ip}]:{settings.port}/api/storage/local")
                for ip in node_ips
                if ip not in seen_hosts
            ],
            return_exceptions=True,
        )
    for ip, r in zip([ip for ip in node_ips if ip not in seen_hosts], results):
        if isinstance(r, Exception) or r.status_code != 200:
            continue
        for entry in r.json():
            locations.append({**entry, "node_name": ip_to_name.get(entry["host"], entry["host"])})

    return locations


@router.delete("/api/disks/{disk_name}/storage")
async def disable_storage(disk_name: str):
    try:
        await asyncio.to_thread(_do_disable_storage, disk_name)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return {"ok": True}
