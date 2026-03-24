import asyncio
import logging
import os

from node_agent import config
from node_agent.disks import discover_disks, mark_data_written
from node_agent.k3s import scale_deployment

log = logging.getLogger("health")

_prev_free: dict[str, int] = {}


async def health_loop() -> None:
    while True:
        try:
            _check_disks()
        except Exception as e:
            log.warning("health check error: %s", e)
        await asyncio.sleep(10)


def _check_disks() -> None:
    if config.YOLAB_PLATFORM == "wsl":
        return

    for disk in discover_disks():
        if disk["status"] != "registered":
            continue
        disk_id = disk["disk_id"]
        free = disk.get("free_bytes")

        if free is None:
            _handle_offline(disk)
            continue

        prev = _prev_free.get(disk_id)
        if prev is not None and free < prev:
            mark_data_written(disk_id)

        _prev_free[disk_id] = free


def _handle_offline(disk: dict) -> None:
    disk_id = disk["disk_id"]
    if disk.get("data_written"):
        _stop_services_using(disk_id)
        log.warning("disk %s offline with data — stopped dependent deployments", disk_id)
    else:
        log.info("disk %s offline, no data written — services unaffected", disk_id)


def _stop_services_using(disk_id: str) -> None:
    volumes_root = "/yolab/volumes"
    if not os.path.isdir(volumes_root):
        return
    import json
    for svc in os.listdir(volumes_root):
        svc_dir = os.path.join(volumes_root, svc)
        if not os.path.isdir(svc_dir):
            continue
        for fname in os.listdir(svc_dir):
            if not fname.endswith(".json"):
                continue
            with open(os.path.join(svc_dir, fname)) as f:
                meta = json.load(f)
            if any(disk_id in p for p in meta.get("disk_paths", [])):
                scale_deployment(svc, 0)
