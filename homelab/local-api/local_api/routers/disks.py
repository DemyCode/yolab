import asyncio
import json
import os
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


def _mgr_pod() -> str:
    result = subprocess.run(
        ["kubectl", "get", "pod", "-n", CEPH_NAMESPACE, "-l", "app=rook-ceph-mgr",
         "-o", "jsonpath={.items[0].metadata.name}"],
        capture_output=True, text=True, timeout=10,
    )
    name = result.stdout.strip()
    if result.returncode != 0 or not name:
        raise RuntimeError("No rook-ceph-mgr pod found")
    return name


def _ceph_osd_map() -> dict[str, int]:
    """Returns {device_name: osd_id} from Ceph OSD metadata."""
    try:
        result = subprocess.run(
            [
                "kubectl", "exec", "-n", CEPH_NAMESPACE, _mgr_pod(), "--",
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


# ── System-disk OSD (sparse loop-file on root filesystem) ─────────────────────

OSD_IMG = "/var/lib/rook/system-osd.img"
OSD_LINK = "/dev/ceph-system-osd"


def _img_size_bytes() -> int | None:
    """Return the size of the OSD image file in bytes, or None if absent."""
    try:
        return os.path.getsize(OSD_IMG)
    except OSError:
        return None


def _fs_free_bytes() -> int:
    """Free bytes on the root filesystem (space available to extend the image)."""
    import shutil
    return shutil.disk_usage("/").free


def _loop_device() -> str | None:
    """Return the loop device currently backing the OSD image, if any."""
    result = subprocess.run(
        ["losetup", "-j", OSD_IMG],
        capture_output=True, text=True, timeout=5,
    )
    line = result.stdout.strip()
    return line.split(":")[0] if line else None


def _ceph_osd_id_for_img() -> int | None:
    """Find the Ceph OSD ID backed by the system-disk loop device, if any."""
    loop = _loop_device()
    if loop is None:
        return None
    loop_name = os.path.basename(loop)  # e.g. "loop0"
    try:
        result = subprocess.run(
            [
                "kubectl", "exec", "-n", CEPH_NAMESPACE, _mgr_pod(), "--",
                "ceph", "osd", "metadata", "--format", "json",
            ],
            capture_output=True, text=True, timeout=15,
        )
        if result.returncode != 0:
            return None
        for osd in json.loads(result.stdout):
            for dev in osd.get("devices", "").split(","):
                if loop_name in dev.strip():
                    return int(osd["id"])
    except Exception:
        pass
    return None


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

    # Flush kernel partition table cache so Rook's udev discovery sees the clean disk immediately
    await asyncio.to_thread(
        subprocess.run,
        ["partprobe", f"/dev/{body.disk_name}"],
        capture_output=True,
    )

    return {"ok": True}


@router.get("/disks/system-osd")
async def system_osd_status():
    img_bytes, free_bytes = await asyncio.gather(
        asyncio.to_thread(_img_size_bytes),
        asyncio.to_thread(_fs_free_bytes),
    )
    osd_id = await asyncio.to_thread(_ceph_osd_id_for_img) if img_bytes is not None else None
    return {
        "exists": img_bytes is not None,
        "size_bytes": img_bytes,
        "fs_free_bytes": free_bytes,
        "ceph_osd_id": osd_id,
    }


_SIZE_UNITS = {"K": 1024, "M": 1024**2, "G": 1024**3, "T": 1024**4, "P": 1024**5}


def _parse_lvm_size(s: str) -> int:
    """Parse an LVM size string like '400G' or '1.5T' to bytes (1024-based)."""
    s = s.strip().upper().rstrip("I")  # strip trailing 'i' (GiB → G)
    for unit, mult in _SIZE_UNITS.items():
        if s.endswith(unit):
            return int(float(s[:-len(unit)]) * mult)
    raise ValueError(f"Unrecognised size: {s!r}")


async def _purge_osd(osd_id: int) -> None:
    """Best-effort Ceph OSD removal before shrinking the backing file."""
    try:
        mgr = await asyncio.to_thread(_mgr_pod)
        for cmd in [
            ["ceph", "osd", "out", str(osd_id)],
            ["ceph", "osd", "crush", "remove", f"osd.{osd_id}"],
            ["ceph", "auth", "del", f"osd.{osd_id}"],
            ["ceph", "osd", "rm", str(osd_id)],
        ]:
            subprocess.run(
                ["kubectl", "exec", "-n", CEPH_NAMESPACE, mgr, "--"] + cmd,
                capture_output=True, timeout=20,
            )
    except Exception:
        pass


class SystemOsdResize(BaseModel):
    size: str  # new total size: "400G", "1.5T", etc.


@router.patch("/disks/system-osd")
async def system_osd_resize(body: SystemOsdResize):
    import re
    if not re.fullmatch(r"\d+(\.\d+)?[KMGTPE]i?", body.size, re.IGNORECASE):
        raise HTTPException(422, "Invalid size — use e.g. 200G, 1.5T")

    current = await asyncio.to_thread(_img_size_bytes)
    if current is None:
        raise HTTPException(404, "System OSD image not found")

    try:
        target = _parse_lvm_size(body.size)
    except ValueError as e:
        raise HTTPException(422, str(e))

    if target > current:
        # Extend: grow sparse file, notify the loop device
        r = await asyncio.to_thread(
            subprocess.run, ["truncate", "-s", body.size, OSD_IMG],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            raise HTTPException(500, f"truncate failed: {r.stderr.strip()}")
        loop = await asyncio.to_thread(_loop_device)
        if loop:
            await asyncio.to_thread(
                subprocess.run, ["losetup", "-c", loop], capture_output=True,
            )
        return {"ok": True, "operation": "extended"}

    if target < current:
        # Shrink: purge OSD, detach loop, truncate file, re-attach
        osd_id = await asyncio.to_thread(_ceph_osd_id_for_img)
        if osd_id is not None:
            await _purge_osd(osd_id)
        loop = await asyncio.to_thread(_loop_device)
        if loop:
            await asyncio.to_thread(
                subprocess.run, ["losetup", "-d", loop], capture_output=True,
            )
        r = await asyncio.to_thread(
            subprocess.run, ["truncate", "-s", body.size, OSD_IMG],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            raise HTTPException(500, f"truncate failed: {r.stderr.strip()}")
        r2 = await asyncio.to_thread(
            subprocess.run, ["losetup", "-f", "--show", OSD_IMG],
            capture_output=True, text=True,
        )
        if r2.returncode == 0:
            new_loop = r2.stdout.strip()
            await asyncio.to_thread(
                subprocess.run, ["ln", "-sf", new_loop, OSD_LINK], capture_output=True,
            )
        return {"ok": True, "operation": "shrunk"}

    return {"ok": True, "operation": "unchanged"}
