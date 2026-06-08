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
    try:
        data = json.loads(kubectl.ceph_exec("osd", "metadata", "--format", "json"))
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


OSD_IMG = "/var/lib/rook/system-osd.img"


def _img_size_bytes() -> int | None:
    try:
        return os.path.getsize(OSD_IMG)
    except OSError:
        return None


def _fs_free_bytes() -> int:
    import shutil
    return shutil.disk_usage("/").free


def _loop_device() -> str | None:
    result = subprocess.run(
        ["losetup", "-j", OSD_IMG],
        capture_output=True, text=True, timeout=5,
    )
    line = result.stdout.strip()
    return line.split(":")[0] if line else None


def _ceph_osd_id_for_img() -> int | None:
    if _loop_device() is None:
        return None
    try:
        r = subprocess.run(
            ["kubectl", "get", "deploy", "-n", CEPH_NAMESPACE, "-l", "app=rook-ceph-osd",
             "-o", "jsonpath={.items[0].metadata.labels.ceph-osd-id}"],
            capture_output=True, text=True, timeout=10,
        )
        val = r.stdout.strip()
        return int(val) if r.returncode == 0 and val.isdigit() else None
    except Exception:
        return None


def _ceph_osd_count() -> int:
    try:
        r = subprocess.run(
            ["kubectl", "get", "deploy", "-n", CEPH_NAMESPACE, "-l", "app=rook-ceph-osd",
             "-o", "jsonpath={.items[*].status.readyReplicas}"],
            capture_output=True, text=True, timeout=10,
        )
        return sum(int(x) for x in r.stdout.split() if x.isdigit()) if r.returncode == 0 else 0
    except Exception:
        return 0


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
    osd_map = await asyncio.to_thread(_ceph_osd_map)
    if body.disk_name in osd_map:
        raise HTTPException(400, "Disk is already a Ceph OSD")

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

    await asyncio.to_thread(
        subprocess.run,
        ["partprobe", f"/dev/{body.disk_name}"],
        capture_output=True,
    )

    # Rook ignores deviceFilter when an explicit devices[] list is present.
    # Patch the CephCluster to add the newly formatted disk so the next prepare
    # job will claim it.  The patch is transient — K3s will revert it on the
    # next manifest reconcile — but the OSD deployment persists independently
    # once Rook has provisioned it.
    await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "patch", "cephcluster", "-n", "rook-ceph", "rook-ceph",
         "--type", "json",
         "-p", json.dumps([{"op": "add", "path": "/spec/storage/devices/-",
                            "value": {"name": body.disk_name}}])],
        capture_output=True,
    )
    # Delete stale prepare job so Rook starts a fresh one immediately.
    await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "delete", "job", "-n", "rook-ceph", "rook-ceph-osd-prepare-homelab",
         "--ignore-not-found"],
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
    s = s.strip().upper().rstrip("I")
    for unit, mult in _SIZE_UNITS.items():
        if s.endswith(unit):
            return int(float(s[:-len(unit)]) * mult)
    raise ValueError(f"Unrecognised size: {s!r}")


async def _purge_osd(osd_id: int) -> None:
    try:
        mgr = await asyncio.to_thread(kubectl.ceph_mgr_pod)
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
    size: str


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
        osd_id = await asyncio.to_thread(_ceph_osd_id_for_img)
        if osd_id is not None:
            osd_count = await asyncio.to_thread(_ceph_osd_count)
            if osd_count <= 1:
                raise HTTPException(400, "Cannot shrink: this is the only Ceph OSD — all data would be lost")
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
        await asyncio.to_thread(
            subprocess.run, ["losetup", "/dev/loop0", OSD_IMG], capture_output=True,
        )
        return {"ok": True, "operation": "shrunk"}

    return {"ok": True, "operation": "unchanged"}
