import asyncio
import logging

from node_agent import config
from node_agent.disks import discover_disks, mark_data_written

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
            log.warning("disk %s offline", disk_id)
            continue

        prev = _prev_free.get(disk_id)
        if prev is not None and free < prev:
            mark_data_written(disk_id)

        _prev_free[disk_id] = free
