import asyncio
import json
import pathlib
import socket
import subprocess

import httpx
from fastapi import APIRouter

from local_api import kubectl
from local_api import priority as priority_module
from local_api.constants import CEPH_CLUSTER_NAME, CEPH_NAMESPACE
from local_api.models.ceph import OsdUsage
from local_api.models.common import OkResponse
from local_api.models.disk import DiskItem, DiskOrderEntry, DiskOrderRequest
from local_api.settings import settings

router = APIRouter()


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


def _ceph_osd_map() -> dict[str, int]:
    mapping: dict[str, int] = {}
    try:
        result = subprocess.run(
            ["kubectl", "get", "pods", "-n", CEPH_NAMESPACE,
             "-l", "app=rook-ceph-osd", "-o", "json"],
            capture_output=True, text=True, timeout=10, check=True,
        )
        for pod in json.loads(result.stdout).get("items", []):
            osd_id_str = pod["metadata"]["labels"].get("ceph-osd-id")
            if osd_id_str is None:
                continue
            for vol in pod["spec"].get("volumes", []):
                if vol.get("name") == "activate-osd":
                    data_dir = (vol.get("hostPath") or {}).get("path", "")
                    if not data_dir:
                        continue
                    try:
                        device = pathlib.Path(data_dir, "block").resolve().name
                        if device:
                            mapping[device] = int(osd_id_str)
                    except Exception:
                        pass
    except Exception:
        pass

    if mapping:
        return mapping

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
    except Exception:
        pass
    return mapping


def _pg_count_for_osd(osd_id: int) -> int:
    try:
        return kubectl.ceph_osd_numpg().get(osd_id, 0)
    except Exception:
        return 0


async def _osd_usage_safe() -> dict[int, OsdUsage]:
    try:
        return await asyncio.wait_for(asyncio.to_thread(kubectl.ceph_osd_df), timeout=5.0)
    except Exception:
        return {}


def _node_ips() -> list[str]:
    ips = {settings.yolab_node_ipv6}
    try:
        for node in kubectl.get_nodes():
            for addr in node["status"]["addresses"]:
                if addr["type"] == "InternalIP" and ":" in addr["address"]:
                    ips.add(addr["address"])
                    break
    except Exception:
        pass
    return list(ips)


async def _gather_from_nodes(path: str) -> list[tuple[str, list]]:
    ips = await asyncio.to_thread(_node_ips)
    async with httpx.AsyncClient(timeout=15) as client:
        results = await asyncio.gather(
            *[client.get(f"http://[{ip}]:{settings.port}{path}") for ip in ips],
            return_exceptions=True,
        )
    return [
        (ip, r.json())
        for ip, r in zip(ips, results)
        if isinstance(r, httpx.Response) and r.status_code == 200
    ]


def _do_activate_local(disk_name: str) -> None:
    # If Rook is currently running a prepare job, don't interfere.
    job_r = subprocess.run(
        ["kubectl", "get", "job", "-n", CEPH_NAMESPACE,
         f"rook-ceph-osd-prepare-{socket.gethostname()}",
         "-o", "jsonpath={.status.active}"],
        capture_output=True, text=True, timeout=10,
    )
    if job_r.returncode == 0 and job_r.stdout.strip() == "1":
        return

    # Wipe stale ceph signatures so Rook can claim the device cleanly.
    # This handles reinstalls where the disk has data from a previous cluster.
    # Skip loop devices — those back the built-in sparse-file OSD.
    if not disk_name.startswith("loop"):
        subprocess.run(
            ["wipefs", "--all", "--force", f"/dev/{disk_name}"],
            capture_output=True,
        )

    r = subprocess.run(
        ["kubectl", "get", "cephcluster", "-n", CEPH_NAMESPACE, CEPH_CLUSTER_NAME,
         "-o", "jsonpath={.spec.storage.devices}"],
        capture_output=True, text=True, timeout=10,
    )
    raw = r.stdout.strip() if r.returncode == 0 else ""
    try:
        existing = json.loads(raw) if raw else []
    except Exception:
        existing = []
    if not any(d.get("name") == disk_name for d in existing):
        new_devices = existing + [{"name": disk_name}]
        subprocess.run(
            ["kubectl", "patch", "cephcluster", "-n", CEPH_NAMESPACE, CEPH_CLUSTER_NAME,
             "--type", "merge",
             "-p", json.dumps({"spec": {"storage": {"devices": new_devices}}})],
            capture_output=True,
        )
    subprocess.run(
        ["kubectl", "delete", "job", "-n", CEPH_NAMESPACE,
         f"rook-ceph-osd-prepare-{socket.gethostname()}", "--ignore-not-found"],
        capture_output=True,
    )


def _do_deactivate_local(disk_name: str) -> None:
    subprocess.run(
        ["wipefs", "--all", "--force", f"/dev/{disk_name}"],
        capture_output=True,
    )
    r = subprocess.run(
        ["kubectl", "get", "cephcluster", "-n", CEPH_NAMESPACE, CEPH_CLUSTER_NAME,
         "-o", "jsonpath={.spec.storage.devices}"],
        capture_output=True, text=True, timeout=10,
    )
    raw = r.stdout.strip() if r.returncode == 0 else ""
    try:
        devices = json.loads(raw) if raw else []
    except Exception:
        return
    new_devices = [d for d in devices if d.get("name") != disk_name]
    if len(new_devices) == len(devices):
        return
    subprocess.run(
        ["kubectl", "patch", "cephcluster", "-n", CEPH_NAMESPACE, CEPH_CLUSTER_NAME,
         "--type", "merge",
         "-p", json.dumps({"spec": {"storage": {"devices": new_devices}}})],
        capture_output=True,
    )


async def _activate_disk(disk_name: str, host: str) -> None:
    if host != settings.yolab_node_ipv6:
        async with httpx.AsyncClient(timeout=30) as client:
            await client.post(
                f"http://[{host}]:{settings.port}/api/disks/activate-local",
                json={"host": host, "disk_name": disk_name},
            )
        return
    await asyncio.to_thread(_do_activate_local, disk_name)


async def _drain_osd(disk_name: str, osd_id: int, host: str) -> None:
    def ceph_reweight() -> bool:
        try:
            kubectl.ceph_exec("osd", "reweight", str(osd_id), "0")
            return True
        except Exception:
            return False

    def ceph_out() -> None:
        try:
            kubectl.ceph_exec("osd", "out", str(osd_id))
        except Exception:
            pass

    def ceph_purge() -> None:
        for cmd in [
            ("osd", "crush", "remove", f"osd.{osd_id}"),
            ("osd", "auth", "del", f"osd.{osd_id}"),
            ("osd", "rm", str(osd_id)),
        ]:
            try:
                kubectl.ceph_exec(*cmd)
            except Exception:
                pass

    try:
        reweighted = await asyncio.to_thread(ceph_reweight)
        if not reweighted:
            return

        await asyncio.to_thread(ceph_out)

        for _ in range(200):
            await asyncio.sleep(5)
            if await asyncio.to_thread(_pg_count_for_osd, osd_id) == 0:
                break
        else:
            return

        await asyncio.to_thread(ceph_purge)

        subprocess.run(
            ["kubectl", "delete", "deploy", "-n", CEPH_NAMESPACE,
             f"rook-ceph-osd-{osd_id}", "--ignore-not-found"],
            capture_output=True,
        )

        if host != settings.yolab_node_ipv6:
            async with httpx.AsyncClient(timeout=30) as client:
                await client.post(
                    f"http://[{host}]:{settings.port}/api/disks/deactivate-local",
                    json={"host": host, "disk_name": disk_name},
                )
        else:
            await asyncio.to_thread(_do_deactivate_local, disk_name)
    except Exception:
        pass


async def _reconcile_storage() -> None:
    from local_api.routers.ceph import _cluster_status_from_k8s

    node_results = await _gather_from_nodes("/api/disks/local")
    disk_map: dict[tuple[str, str], dict] = {
        (d["host"], d["name"]): d
        for _, node_disks in node_results
        for d in node_disks
    }

    priority = await asyncio.to_thread(priority_module.read)
    known = {(e.host, e.disk_name) for e in priority}
    updated = False
    for (host, name), disk in disk_map.items():
        if (host, name) not in known:
            if disk.get("is_builtin"):
                await asyncio.to_thread(priority_module.prepend, host, name)
            else:
                await asyncio.to_thread(priority_module.append, host, name)
            updated = True
    if updated:
        priority = await asyncio.to_thread(priority_module.read)

    if not priority:
        return

    any_osd = any(d.get("is_osd") for d in disk_map.values())
    if not any_osd:
        for entry in priority:
            disk = disk_map.get((entry.host, entry.disk_name))
            if disk is not None:
                await _activate_disk(entry.disk_name, entry.host)
                return
        return

    try:
        status = await asyncio.to_thread(_cluster_status_from_k8s)
        current_used = status.get("ceph", {}).get("capacity", {}).get("bytesUsed", 0)
    except Exception:
        return

    if current_used == 0:
        return

    demanded_space = current_used * 1.2
    running_space = 0
    needed: set[tuple[str, str]] = set()

    for entry in priority:
        disk = disk_map.get((entry.host, entry.disk_name))
        if disk is None:
            continue
        if running_space < demanded_space:
            needed.add((entry.host, entry.disk_name))
            running_space += disk.get("size_bytes", 0)

    osd_map = await asyncio.to_thread(_ceph_osd_map)

    # Only drain if every disk in `needed` is already an active OSD.
    # If any needed disk isn't ready yet, draining an existing OSD would leave
    # data with nowhere to migrate.
    needed_all_ready = all(disk_map.get(k, {}).get("is_osd", False) for k in needed)

    for entry in priority:
        key = (entry.host, entry.disk_name)
        disk = disk_map.get(key)
        if disk is None:
            continue
        is_osd = disk.get("is_osd", False)
        if key in needed and not is_osd:
            await _activate_disk(entry.disk_name, entry.host)
        elif key not in needed and is_osd and needed_all_ready:
            osd_id = osd_map.get(disk["name"])
            if osd_id is not None:
                asyncio.create_task(_drain_osd(entry.disk_name, osd_id, entry.host))


@router.get("/disks/local", response_model=list[DiskItem])
async def disks_local() -> list[DiskItem]:
    devices, osd_map, osd_usage = await asyncio.gather(
        asyncio.to_thread(_lsblk),
        asyncio.to_thread(_ceph_osd_map),
        _osd_usage_safe(),
    )

    hostname = socket.gethostname()
    result = []

    for d in devices:
        name = d["name"]
        dtype = d.get("type")
        is_loop = dtype == "loop"
        is_disk = dtype == "disk"

        if not is_disk and not is_loop:
            continue
        if is_disk and _is_system_disk(d):
            continue

        model = (d.get("model") or "").strip()
        if is_loop:
            model = "Built-in storage"

        osd_id = osd_map.get(name)
        usage = osd_usage.get(osd_id) if osd_id is not None else None

        result.append(DiskItem(
            name=name,
            model=model,
            size_bytes=int(d.get("size") or 0),
            host=settings.yolab_node_ipv6,
            hostname=hostname,
            is_osd=osd_id is not None,
            is_builtin=is_loop,
            used_bytes=usage.used_bytes if usage else None,
            free_bytes=usage.free_bytes if usage else None,
        ))

    return result


@router.get("/disks", response_model=list[DiskItem])
async def disks() -> list[DiskItem]:
    node_results, priority = await asyncio.gather(
        _gather_from_nodes("/api/disks/local"),
        asyncio.to_thread(priority_module.read),
    )

    disk_map: dict[tuple[str, str], dict] = {
        (d["host"], d["name"]): d
        for _, node_disks in node_results
        for d in node_disks
    }

    known = {(e.host, e.disk_name) for e in priority}
    updated = False
    for (host, name), disk in disk_map.items():
        if (host, name) not in known:
            if disk.get("is_builtin"):
                await asyncio.to_thread(priority_module.prepend, host, name)
            else:
                await asyncio.to_thread(priority_module.append, host, name)
            updated = True
    if updated:
        priority = await asyncio.to_thread(priority_module.read)

    result = []
    for entry in priority:
        disk = disk_map.get((entry.host, entry.disk_name))
        if disk is None:
            continue
        result.append(DiskItem(**disk))

    return result


@router.put("/disks/order", response_model=OkResponse)
async def update_order(body: DiskOrderRequest) -> OkResponse:
    entries = [
        priority_module.PriorityEntry(host=e.host, disk_name=e.disk_name)
        for e in body.entries
    ]
    await asyncio.to_thread(priority_module.write, entries)
    asyncio.create_task(_reconcile_storage())
    return OkResponse()


@router.post("/disks/activate-local", response_model=OkResponse)
async def activate_local(body: DiskOrderEntry) -> OkResponse:
    await asyncio.to_thread(_do_activate_local, body.disk_name)
    return OkResponse()


@router.post("/disks/deactivate-local", response_model=OkResponse)
async def deactivate_local(body: DiskOrderEntry) -> OkResponse:
    await asyncio.to_thread(_do_deactivate_local, body.disk_name)
    return OkResponse()
