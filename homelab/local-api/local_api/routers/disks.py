import asyncio
import json
import os
import pathlib
import socket
import subprocess

import httpx
from fastapi import APIRouter, HTTPException

from local_api import kubectl
from local_api import priority as priority_module
from local_api.constants import CEPH_CLUSTER_NAME, CEPH_NAMESPACE, LOOP_DEVICE
from local_api.models.common import OkResponse
from local_api.models.disk import (
    AddToStorageRequest,
    DiskInfo,
    DiskState,
    EjectRequest,
    EjectStatus,
    PriorityItem,
    PriorityUpdateRequest,
    SystemOsdInfo,
    SystemOsdResize,
    SystemOsdResizeResponse,
)
from local_api.settings import settings

router = APIRouter()

# Ejection-done signals live in /run (tmpfs) so they survive FastAPI restarts
# within the same boot but are cleaned up on reboot.
_EJECT_DONE_DIR = pathlib.Path("/run/yolab/eject-done")


# ── lsblk / disk detection ────────────────────────────────────────────────────

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


def _first_fstype(device: dict) -> str | None:
    if device.get("fstype"):
        return device["fstype"]
    for child in device.get("children") or []:
        if child.get("fstype"):
            return child["fstype"]
    return None


# ── Ceph OSD mapping ──────────────────────────────────────────────────────────

def _ceph_osd_map() -> dict[str, int]:
    """Returns {device_name: osd_id} by reading pod volumes. No ceph auth needed.

    If a disk was renamed after Rook provisioned it (e.g. sdc → sdb on reconnect),
    the block symlink in the OSD data dir will point to the old name. We detect this
    and re-map to the real ceph_bluestore disk.
    """
    mapping: dict[str, int] = {}
    try:
        result = subprocess.run(
            ["kubectl", "get", "pods", "-n", CEPH_NAMESPACE,
             "-l", "app=rook-ceph-osd", "-o", "json"],
            capture_output=True, text=True, timeout=10, check=True,
        )
        pods = json.loads(result.stdout).get("items", [])
        for pod in pods:
            osd_id_str = pod["metadata"]["labels"].get("ceph-osd-id")
            if osd_id_str is None:
                continue
            for vol in pod["spec"].get("volumes", []):
                if vol.get("name") == "activate-osd":
                    data_dir = (vol.get("hostPath") or {}).get("path", "")
                    if not data_dir:
                        continue
                    block_link = pathlib.Path(data_dir) / "block"
                    try:
                        device = block_link.resolve().name
                        if device:
                            mapping[device] = int(osd_id_str)
                    except Exception:
                        pass
    except Exception:
        pass

    if mapping:
        # Detect phantom entries: mapped device name no longer exists (disk renamed).
        # Find ceph_bluestore disks not yet claimed by a valid mapping and re-map.
        # NOTE: use /dev/ existence, not lsblk type="disk", so loop devices like
        # loop0 are not mistakenly treated as phantoms.
        try:
            devices = _lsblk()
            known = {d["name"] for d in devices if pathlib.Path(f"/dev/{d['name']}").exists()}
            valid = {dev for dev in mapping if dev in known}
            phantoms = {dev: oid for dev, oid in mapping.items() if dev not in known}
            if phantoms:
                # Disks with bluestore content that aren't already correctly mapped
                orphaned_bluestore = [
                    d["name"] for d in devices
                    if d.get("type") == "disk"
                    and _first_fstype(d) == "ceph_bluestore"
                    and d["name"] not in valid
                ]
                for (_, osd_id), real_dev in zip(
                    sorted(phantoms.items()), sorted(orphaned_bluestore)
                ):
                    mapping[real_dev] = osd_id
                for old_dev in phantoms:
                    mapping.pop(old_dev, None)
        except Exception:
            pass
        return mapping

    # Fallback via ceph osd metadata
    try:
        data = json.loads(kubectl.ceph_exec("osd", "metadata", "--format", "json"))
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


def _pg_count_for_osd(osd_id: int) -> int:
    try:
        r = subprocess.run(
            ["kubectl", "exec", "-n", CEPH_NAMESPACE,
             kubectl.ceph_mgr_pod(), "--",
             "ceph", "pg", "ls-by-osd", str(osd_id), "--format", "json"],
            capture_output=True, text=True, timeout=20,
        )
        if r.returncode != 0:
            return 0
        return len(json.loads(r.stdout).get("pg_stats", []))
    except Exception:
        return 0


# ── multi-node fan-out ────────────────────────────────────────────────────────

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


# ── disk list endpoints ───────────────────────────────────────────────────────

@router.get("/disks/local", response_model=list[DiskInfo])
async def disks_local() -> list[DiskInfo]:
    devices, osd_map, osd_usage, queued = await asyncio.gather(
        asyncio.to_thread(_lsblk),
        asyncio.to_thread(_ceph_osd_map),
        asyncio.to_thread(kubectl.ceph_osd_df),
        asyncio.to_thread(priority_module.read),
    )

    my_host = settings.yolab_node_ipv6
    waiting_names = {e.disk_name for e in queued if e.host == my_host}
    # Global priority position per disk name (for this node's disks)
    queue_positions = {
        e.disk_name: i + 1
        for i, e in enumerate(queued)
        if e.host == my_host
    }

    # Auto-append blank disks to the priority list (invisible storage expansion).
    newly_found = [
        d["name"] for d in devices
        if d.get("type") == "disk"
        and not _is_system_disk(d)
        and d["name"] not in osd_map
        and _first_fstype(d) is None
        and d["name"] not in waiting_names
        and not priority_module.is_rejected(my_host, d["name"])
    ]
    for name in newly_found:
        priority_module.append(my_host, name)
    if newly_found:
        queued = priority_module.read()
        waiting_names = {e.disk_name for e in queued if e.host == my_host}
        queue_positions = {e.disk_name: i + 1 for i, e in enumerate(queued) if e.host == my_host}

    hostname = socket.gethostname()
    raw: list[dict] = []
    for d in devices:
        if d.get("type") != "disk":
            continue
        name = d["name"]
        base = dict(
            name=name,
            model=(d.get("model") or "").strip(),
            size_bytes=int(d.get("size") or 0),
            host=settings.yolab_node_ipv6,
            hostname=hostname,
        )

        if _is_system_disk(d):
            raw.append({**base, "state": DiskState.SYSTEM, "fs_type": _first_fstype(d)})
        elif name in osd_map:
            osd_id = osd_map[name]
            usage = osd_usage.get(osd_id)
            is_ejecting = usage is not None and usage.reweight == 0.0
            raw.append({**base,
                        "state": DiskState.EJECTING if is_ejecting else DiskState.ACTIVE,
                        "ceph_osd_id": osd_id,
                        "used_bytes": usage.used_bytes if usage else None,
                        "free_bytes": usage.free_bytes if usage else None})
        elif name in waiting_names:
            raw.append({**base, "state": DiskState.WAITING,
                        "queue_position": queue_positions[name]})
        else:
            raw.append({**base, "state": DiskState.UNFORMATTED, "fs_type": _first_fstype(d)})

    # Compute can_eject: disk's used data must fit in the free space of all other active disks
    active = [d for d in raw if d["state"] == DiskState.ACTIVE]
    result = []
    for d in raw:
        can_eject = False
        if d["state"] == DiskState.ACTIVE and d.get("used_bytes") is not None:
            other_free = sum(
                o["free_bytes"] for o in active
                if o["name"] != d["name"] and o.get("free_bytes") is not None
            )
            can_eject = d["used_bytes"] <= other_free
        result.append(DiskInfo(**d, can_eject=can_eject))

    return result


@router.get("/disks", response_model=list[DiskInfo])
async def disks() -> list[DiskInfo]:
    return [
        DiskInfo(**disk) if isinstance(disk, dict) else DiskInfo.model_validate(disk)
        for _, node_disks in await _gather_from_nodes("/api/disks/local")
        for disk in node_disks
    ]


# ── add to storage ────────────────────────────────────────────────────────────

@router.post("/disks/add", response_model=OkResponse)
async def add_disk(body: AddToStorageRequest) -> OkResponse:
    if body.host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=60) as client:
            r = await client.post(
                f"http://[{body.host}]:{settings.port}/api/disks/add",
                json=body.model_dump(),
            )
        if r.status_code != 200:
            raise HTTPException(r.status_code, r.json().get("detail", "Failed"))
        return OkResponse.model_validate(r.json())

    devices = await asyncio.to_thread(_lsblk)
    disk = next((d for d in devices if d["name"] == body.disk_name), None)
    if not disk:
        raise HTTPException(404, "Disk not found")
    if _is_system_disk(disk):
        raise HTTPException(400, "Cannot add system disk to storage")
    osd_map = await asyncio.to_thread(_ceph_osd_map)
    if body.disk_name in osd_map:
        raise HTTPException(400, "Disk is already active storage")

    # Only wipe if the disk has existing data — blank disks need no wipe.
    if _first_fstype(disk) is not None:
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
            raise HTTPException(500, f"Wipe failed: {r1.stderr.strip()}")
        if r2.returncode != 0:
            raise HTTPException(500, f"Partition table clear failed: {r2.stderr.strip()}")
        await asyncio.to_thread(
            subprocess.run, ["partprobe", f"/dev/{body.disk_name}"], capture_output=True,
        )

    await asyncio.to_thread(priority_module.unreject, body.host, body.disk_name)
    await asyncio.to_thread(priority_module.append, body.host, body.disk_name)

    # Activate immediately if cluster is already above threshold
    await _maybe_activate()

    return OkResponse()


# ── queue management ──────────────────────────────────────────────────────────

@router.delete("/disks/queue/{disk_name}", response_model=OkResponse)
async def remove_from_queue(disk_name: str, host: str) -> OkResponse:
    if host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=10) as client:
            await client.delete(
                f"http://[{host}]:{settings.port}/api/disks/queue/{disk_name}",
                params={"host": host},
            )
        return OkResponse()
    await asyncio.to_thread(priority_module.remove, host, disk_name)
    return OkResponse()


# ── priority endpoints ────────────────────────────────────────────────────────

@router.get("/disks/priority", response_model=list[PriorityItem])
async def get_priority() -> list[PriorityItem]:
    priority_list, node_results = await asyncio.gather(
        asyncio.to_thread(priority_module.read),
        _gather_from_nodes("/api/disks/local"),
    )
    disk_map = {
        (d["host"], d["name"]): d
        for _, disks in node_results
        for d in disks
    }
    result = []
    for i, entry in enumerate(priority_list):
        disk = disk_map.get((entry.host, entry.disk_name), {})
        result.append(PriorityItem(
            host=entry.host,
            disk_name=entry.disk_name,
            position=i + 1,
            model=disk.get("model", ""),
            size_bytes=disk.get("size_bytes", 0),
            state=disk.get("state", "offline"),
            hostname=disk.get("hostname", ""),
            used_bytes=disk.get("used_bytes"),
            free_bytes=disk.get("free_bytes"),
            can_eject=disk.get("can_eject", False),
            ceph_osd_id=disk.get("ceph_osd_id"),
        ))
    return result


@router.put("/disks/priority", response_model=OkResponse)
async def put_priority(body: PriorityUpdateRequest) -> OkResponse:
    entries = [
        priority_module.PriorityEntry(host=e.host, disk_name=e.disk_name)
        for e in body.entries
    ]
    await asyncio.to_thread(priority_module.write, entries)
    return OkResponse()


# ── activation ────────────────────────────────────────────────────────────────

def _do_activate_local(disk_name: str) -> None:
    subprocess.run(
        ["kubectl", "patch", "cephcluster", "-n", CEPH_NAMESPACE, CEPH_CLUSTER_NAME,
         "--type", "json",
         "-p", json.dumps([{"op": "add", "path": "/spec/storage/devices/-",
                            "value": {"name": disk_name}}])],
        capture_output=True,
    )
    subprocess.run(
        ["kubectl", "delete", "job", "-n", CEPH_NAMESPACE,
         f"rook-ceph-osd-prepare-{socket.gethostname()}", "--ignore-not-found"],
        capture_output=True,
    )


@router.post("/disks/activate-local", response_model=OkResponse)
async def activate_local(body: AddToStorageRequest) -> OkResponse:
    """Internal endpoint — called by primary node to activate a disk on this node."""
    await asyncio.to_thread(_do_activate_local, body.disk_name)
    return OkResponse()


async def _activate_disk(disk_name: str, host: str) -> None:
    if host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=60) as client:
            await client.post(
                f"http://[{host}]:{settings.port}/api/disks/activate-local",
                json={"disk_name": disk_name, "host": host},
            )
        return
    await asyncio.to_thread(_do_activate_local, disk_name)


async def _maybe_activate() -> None:
    priority, node_results = await asyncio.gather(
        asyncio.to_thread(priority_module.read),
        _gather_from_nodes("/api/disks/local"),
    )
    disk_map = {
        (d["host"], d["name"]): d
        for _, disks in node_results
        for d in disks
    }
    # Find the first WAITING disk in priority order
    next_disk = next(
        (disk_map[(e.host, e.disk_name)]
         for e in priority
         if (e.host, e.disk_name) in disk_map
         and disk_map[(e.host, e.disk_name)].get("state") == DiskState.WAITING),
        None,
    )
    if not next_disk:
        return
    try:
        from local_api.routers.ceph import _cluster_status_from_k8s
        status = await asyncio.to_thread(_cluster_status_from_k8s)
        cap = status.get("ceph", {}).get("capacity", {})
        total = cap.get("bytesTotal", 0)
        used = cap.get("bytesUsed", 0)
        if total == 0 or (used / total) < settings.disk_activation_threshold:
            return
    except Exception:
        return
    await _activate_disk(next_disk["name"], next_disk["host"])


# ── eject ─────────────────────────────────────────────────────────────────────

@router.post("/disks/eject", response_model=OkResponse)
async def eject_disk(body: EjectRequest) -> OkResponse:
    if body.host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=60) as client:
            r = await client.post(
                f"http://[{body.host}]:{settings.port}/api/disks/eject",
                json=body.model_dump(),
            )
        if r.status_code != 200:
            raise HTTPException(r.status_code, r.json().get("detail", "Failed"))
        return OkResponse.model_validate(r.json())

    osd_map = await asyncio.to_thread(_ceph_osd_map)
    if body.disk_name not in osd_map:
        raise HTTPException(404, "Disk is not active storage")
    osd_id = osd_map[body.disk_name]

    osd_usage = await asyncio.to_thread(kubectl.ceph_osd_df)
    usage = osd_usage.get(osd_id)
    if usage:
        other_free = sum(u.free_bytes for oid, u in osd_usage.items() if oid != osd_id)
        if usage.used_bytes > other_free:
            raise HTTPException(400, {
                "reason": "not_enough_space",
                "used_bytes": usage.used_bytes,
                "other_free_bytes": other_free,
            })

    asyncio.create_task(_drain_osd(body.disk_name, osd_id))
    return OkResponse()


async def _drain_osd(disk_name: str, osd_id: int) -> None:
    def ceph_cmd(*args: str) -> None:
        subprocess.run(
            ["kubectl", "exec", "-n", CEPH_NAMESPACE, kubectl.ceph_mgr_pod(), "--"] + list(args),
            capture_output=True, timeout=30,
        )

    try:
        await asyncio.to_thread(ceph_cmd, "ceph", "osd", "reweight", str(osd_id), "0")
        await asyncio.to_thread(ceph_cmd, "ceph", "osd", "out", str(osd_id))

        for _ in range(200):
            await asyncio.sleep(5)
            if await asyncio.to_thread(_pg_count_for_osd, osd_id) == 0:
                break

        await asyncio.to_thread(ceph_cmd, "ceph", "osd", "crush", "remove", f"osd.{osd_id}")
        await asyncio.to_thread(ceph_cmd, "ceph", "auth", "del", f"osd.{osd_id}")
        await asyncio.to_thread(ceph_cmd, "ceph", "osd", "rm", str(osd_id))

        # Wipe disk so it shows as blank (no stale bluestore signature)
        await asyncio.to_thread(
            subprocess.run, ["wipefs", "--all", "--force", f"/dev/{disk_name}"],
            capture_output=True,
        )
        # Reject so the wiped disk doesn't auto-re-queue — user explicitly ejected it
        priority_module.remove(settings.yolab_node_ipv6, disk_name)
    except Exception:
        pass
    finally:
        # Signal completion in /run (tmpfs — survives FastAPI restart, cleared on reboot)
        _EJECT_DONE_DIR.mkdir(parents=True, exist_ok=True)
        (_EJECT_DONE_DIR / disk_name).touch()


@router.get("/disks/eject/{disk_name}/status", response_model=EjectStatus)
async def eject_status(disk_name: str) -> EjectStatus:
    done_file = _EJECT_DONE_DIR / disk_name
    if done_file.exists():
        done_file.unlink(missing_ok=True)
        return EjectStatus(pg_count=0, done=True, safe_to_unplug=True)

    osd_map = await asyncio.to_thread(_ceph_osd_map)
    if disk_name not in osd_map:
        raise HTTPException(404, "No ejection in progress for this disk")

    osd_id = osd_map[disk_name]
    pg_count = await asyncio.to_thread(_pg_count_for_osd, osd_id)
    return EjectStatus(pg_count=pg_count, done=False, safe_to_unplug=False)


# ── system OSD (loop device) ──────────────────────────────────────────────────

def _img_size_bytes() -> int | None:
    try:
        return os.path.getsize(settings.osd_img_path)
    except OSError:
        return None


def _fs_free_bytes() -> int:
    import shutil
    return shutil.disk_usage("/").free


def _loop_device() -> str | None:
    result = subprocess.run(
        ["losetup", "-j", settings.osd_img_path], capture_output=True, text=True, timeout=5,
    )
    line = result.stdout.strip()
    return line.split(":")[0] if line else None


def _ceph_osd_id_for_img() -> int | None:
    if _loop_device() is None:
        return None
    return _ceph_osd_map().get("loop0")


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


@router.get("/disks/system-osd", response_model=SystemOsdInfo)
async def system_osd_status() -> SystemOsdInfo:
    img_bytes, free_bytes = await asyncio.gather(
        asyncio.to_thread(_img_size_bytes),
        asyncio.to_thread(_fs_free_bytes),
    )
    osd_id = await asyncio.to_thread(_ceph_osd_id_for_img) if img_bytes is not None else None
    return SystemOsdInfo(
        exists=img_bytes is not None,
        size_bytes=img_bytes,
        fs_free_bytes=free_bytes,
        ceph_osd_id=osd_id,
    )


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


@router.patch("/disks/system-osd", response_model=SystemOsdResizeResponse)
async def system_osd_resize(body: SystemOsdResize) -> SystemOsdResizeResponse:
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
            subprocess.run, ["truncate", "-s", body.size, settings.osd_img_path],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            raise HTTPException(500, f"truncate failed: {r.stderr.strip()}")
        loop = await asyncio.to_thread(_loop_device)
        if loop:
            await asyncio.to_thread(subprocess.run, ["losetup", "-c", loop], capture_output=True)
        return SystemOsdResizeResponse(operation="extended")

    if target < current:
        osd_id = await asyncio.to_thread(_ceph_osd_id_for_img)
        if osd_id is not None:
            if await asyncio.to_thread(_ceph_osd_count) <= 1:
                raise HTTPException(
                    400, "Cannot shrink: this is the only storage disk — all data would be lost"
                )
            await _purge_osd(osd_id)
        loop = await asyncio.to_thread(_loop_device)
        if loop:
            await asyncio.to_thread(subprocess.run, ["losetup", "-d", loop], capture_output=True)
        r = await asyncio.to_thread(
            subprocess.run, ["truncate", "-s", body.size, settings.osd_img_path],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            raise HTTPException(500, f"truncate failed: {r.stderr.strip()}")
        await asyncio.to_thread(
            subprocess.run, ["losetup", LOOP_DEVICE, settings.osd_img_path], capture_output=True,
        )
        return SystemOsdResizeResponse(operation="shrunk")

    return SystemOsdResizeResponse(operation="unchanged")
