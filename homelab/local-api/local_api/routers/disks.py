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


# ── System-disk OSD (LVM LV in the 'pool' VG) ─────────────────────────────────

VG_NAME = "pool"
LV_NAME = "ceph"
LV_PATH = f"/dev/{VG_NAME}/{LV_NAME}"


def _vg_free_bytes() -> int:
    """Return free bytes in the pool VG."""
    result = subprocess.run(
        ["vgs", VG_NAME, "--reportformat", "json", "--units", "b", "--nosuffix"],
        capture_output=True, text=True, timeout=10,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    data = json.loads(result.stdout)
    vg = data["report"][0]["vg"][0]
    return int(vg["vg_free"])


def _lv_size_bytes() -> int | None:
    """Return the size of the ceph LV in bytes, or None if it doesn't exist."""
    result = subprocess.run(
        ["lvs", LV_PATH, "--reportformat", "json", "--units", "b", "--nosuffix"],
        capture_output=True, text=True, timeout=10,
    )
    if result.returncode != 0:
        return None
    data = json.loads(result.stdout)
    try:
        return int(data["report"][0]["lv"][0]["lv_size"])
    except (KeyError, IndexError):
        return None


def _ceph_osd_id_for_lv() -> int | None:
    """Find the Ceph OSD ID that is backed by the system-disk LV, if any."""
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
                if LV_NAME in dev or "pool-ceph" in dev or "pool/ceph" in dev:
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
    lv_bytes, free_bytes = await asyncio.gather(
        asyncio.to_thread(_lv_size_bytes),
        asyncio.to_thread(_vg_free_bytes),
    )
    osd_id = await asyncio.to_thread(_ceph_osd_id_for_lv) if lv_bytes is not None else None
    return {
        "exists": lv_bytes is not None,
        "size_bytes": lv_bytes,
        "vg_free_bytes": free_bytes,
        "ceph_osd_id": osd_id,
    }


class SystemOsdCreate(BaseModel):
    size: str  # LVM size string: "200G", "1T", etc.


@router.post("/disks/system-osd")
async def system_osd_create(body: SystemOsdCreate):
    existing = await asyncio.to_thread(_lv_size_bytes)
    if existing is not None:
        raise HTTPException(400, "System OSD LV already exists — delete it first or extend it")

    import re
    if not re.fullmatch(r"\d+(\.\d+)?[KMGTPE]i?", body.size, re.IGNORECASE):
        raise HTTPException(422, "Invalid size format — use e.g. 200G, 1T")

    result = await asyncio.to_thread(
        subprocess.run,
        ["lvcreate", "-L", body.size, "-n", LV_NAME, VG_NAME],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise HTTPException(500, f"lvcreate failed: {result.stderr.strip()}")

    return {"ok": True, "path": LV_PATH}


class SystemOsdResize(BaseModel):
    size: str  # new total size: "500G", "1T", etc.


@router.patch("/disks/system-osd")
async def system_osd_resize(body: SystemOsdResize):
    existing = await asyncio.to_thread(_lv_size_bytes)
    if existing is None:
        raise HTTPException(404, "System OSD LV does not exist")

    import re
    if not re.fullmatch(r"\d+(\.\d+)?[KMGTPE]i?", body.size, re.IGNORECASE):
        raise HTTPException(422, "Invalid size format — use e.g. 500G, 1T")

    result = await asyncio.to_thread(
        subprocess.run,
        ["lvextend", "-L", body.size, LV_PATH],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise HTTPException(500, f"lvextend failed: {result.stderr.strip()}")

    return {"ok": True}


@router.delete("/disks/system-osd")
async def system_osd_delete():
    existing = await asyncio.to_thread(_lv_size_bytes)
    if existing is None:
        raise HTTPException(404, "System OSD LV does not exist")

    osd_id = await asyncio.to_thread(_ceph_osd_id_for_lv)

    # Best-effort Ceph OSD removal — cluster must be healthy enough to accept it
    if osd_id is not None:
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
            pass  # Proceed with LV removal even if Ceph commands fail

    result = await asyncio.to_thread(
        subprocess.run,
        ["lvremove", "-f", LV_PATH],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise HTTPException(500, f"lvremove failed: {result.stderr.strip()}")

    return {"ok": True}
