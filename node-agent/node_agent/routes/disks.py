import uuid
from typing import AsyncIterator

from fastapi import APIRouter, HTTPException
from fastapi.responses import StreamingResponse
from pydantic import BaseModel

from node_agent import config
from node_agent.disks import (
    discover_disks,
    init_block_disk,
    init_directory_disk,
    init_network_disk,
)

router = APIRouter(tags=["disks"])


def _check_platform():
    if config.YOLAB_PLATFORM == "wsl":
        raise HTTPException(status_code=501, detail="Block disk ops not supported on WSL")


async def _sse(gen: AsyncIterator[str]) -> AsyncIterator[bytes]:
    async for line in gen:
        yield f"data: {line}\n\n".encode()


@router.get("/disks")
def list_disks():
    return discover_disks()


async def _init_block_gen(disk_id: str, device: str, label: str | None):
    yield f"$ mkfs.ext4 -F {device}"
    try:
        init_block_disk(disk_id, device, label)
        yield f"Mounted at /yolab/data/{disk_id}"
        yield "[DONE]"
    except Exception as e:
        yield f"[ERROR] {e}"


@router.post("/disks/{disk_id}/init")
def init_disk(disk_id: str):
    _check_platform()
    by_id = {d["disk_id"]: d for d in discover_disks()}
    disk = by_id.get(disk_id)
    if not disk:
        raise HTTPException(status_code=404, detail="Disk not found")
    if disk["status"] != "unformatted":
        raise HTTPException(status_code=400, detail=f"Expected unformatted, got {disk['status']}")

    async def gen():
        async for line in _init_block_gen(disk_id, disk["device"], disk.get("label")):
            yield line

    return StreamingResponse(_sse(gen()), media_type="text/event-stream")


@router.post("/disks/{disk_id}/wipe-init")
def wipe_init_disk(disk_id: str):
    _check_platform()
    by_id = {d["disk_id"]: d for d in discover_disks()}
    disk = by_id.get(disk_id)
    if not disk:
        raise HTTPException(status_code=404, detail="Disk not found")

    async def gen():
        async for line in _init_block_gen(disk_id, disk["device"], disk.get("label")):
            yield line

    return StreamingResponse(_sse(gen()), media_type="text/event-stream")


class DirectoryInitRequest(BaseModel):
    base_path: str
    label: str | None = None


@router.post("/disks/init-directory")
def init_directory(req: DirectoryInitRequest):
    disk_id = str(uuid.uuid4())

    async def gen():
        yield f"$ creating yolab-data directory at {req.base_path}"
        try:
            mount_path = init_directory_disk(disk_id, req.base_path, req.label)
            yield f"Initialized at {mount_path}"
            yield "[DONE]"
        except Exception as e:
            yield f"[ERROR] {e}"

    return StreamingResponse(_sse(gen()), media_type="text/event-stream")


class NetworkDiskInitRequest(BaseModel):
    mount_path: str
    label: str | None = None


@router.post("/disks/{disk_id}/init-network")
def init_network(disk_id: str, req: NetworkDiskInitRequest):
    init_network_disk(disk_id, req.mount_path, req.label)
    return {"disk_id": disk_id, "mount_path": req.mount_path}
