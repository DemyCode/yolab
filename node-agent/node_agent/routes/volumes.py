import json
import os
from typing import AsyncIterator

from fastapi import APIRouter, HTTPException
from fastapi.responses import StreamingResponse
from pydantic import BaseModel

from node_agent import config
from node_agent.mergerfs import (
    create_volume,
    destroy_volume,
    reorganize_estimate,
    reorganize_volume,
    volume_path,
)

router = APIRouter(tags=["volumes"])

VOLUMES_META_ROOT = "/yolab/volumes"


def _check_platform():
    if config.YOLAB_PLATFORM in ("wsl", "darwin"):
        raise HTTPException(
            status_code=501,
            detail=f"mergerfs volumes not supported on {config.YOLAB_PLATFORM}",
        )


def _meta_path(service_name: str, volume_name: str) -> str:
    return f"{VOLUMES_META_ROOT}/{service_name}/{volume_name}.json"


def _read_meta(service_name: str, volume_name: str) -> dict | None:
    p = _meta_path(service_name, volume_name)
    if os.path.exists(p):
        with open(p) as f:
            return json.load(f)
    return None


def _write_meta(service_name: str, volume_name: str, data: dict) -> None:
    p = _meta_path(service_name, volume_name)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as f:
        json.dump(data, f, indent=2)


def _list_volumes() -> list[dict]:
    vols = []
    if not os.path.isdir(VOLUMES_META_ROOT):
        return vols
    for svc in os.listdir(VOLUMES_META_ROOT):
        svc_dir = os.path.join(VOLUMES_META_ROOT, svc)
        if not os.path.isdir(svc_dir):
            continue
        for fname in os.listdir(svc_dir):
            if fname.endswith(".json"):
                with open(os.path.join(svc_dir, fname)) as f:
                    vols.append(json.load(f))
    return vols


class VolumeCreateRequest(BaseModel):
    service_name: str
    volume_name: str
    disk_paths: list[str]


class VolumeReorganizeRequest(BaseModel):
    new_disk_paths: list[str]


@router.get("/volumes")
def list_vols():
    return _list_volumes()


@router.post("/volumes/create")
def create_vol(req: VolumeCreateRequest):
    _check_platform()
    mount = create_volume(req.service_name, req.volume_name, req.disk_paths)
    meta = {
        "service_name": req.service_name,
        "volume_name": req.volume_name,
        "disk_paths": req.disk_paths,
        "mergerfs_path": mount,
        "status": "active",
    }
    _write_meta(req.service_name, req.volume_name, meta)
    return meta


@router.delete("/volumes/{service_name}/{volume_name}")
def delete_vol(service_name: str, volume_name: str):
    _check_platform()
    destroy_volume(service_name, volume_name)
    p = _meta_path(service_name, volume_name)
    if os.path.exists(p):
        os.remove(p)
    return {"deleted": True}


@router.get("/volumes/{service_name}/{volume_name}/reorganize-estimate")
def estimate_reorg(service_name: str, volume_name: str, new_disk_paths: str):
    meta = _read_meta(service_name, volume_name)
    if not meta:
        raise HTTPException(status_code=404, detail="Volume not found")
    new_paths = [p.strip() for p in new_disk_paths.split(",") if p.strip()]
    return reorganize_estimate(meta["disk_paths"], new_paths)


async def _sse_bytes(gen: AsyncIterator[str]) -> AsyncIterator[bytes]:
    async for line in gen:
        yield f"data: {line}\n\n".encode()


@router.post("/volumes/{service_name}/{volume_name}/reorganize")
def reorg_vol(service_name: str, volume_name: str, req: VolumeReorganizeRequest):
    _check_platform()
    meta = _read_meta(service_name, volume_name)
    if not meta:
        raise HTTPException(status_code=404, detail="Volume not found")

    old_paths = meta["disk_paths"]
    new_paths = req.new_disk_paths

    async def gen():
        import subprocess
        yield f"$ docker service scale {service_name}=0"
        subprocess.run(["docker", "service", "scale", f"{service_name}=0"], check=False)

        async for line in reorganize_volume(service_name, volume_name, old_paths, new_paths):
            if line == "[DONE]":
                yield f"$ docker service scale {service_name}=1"
                subprocess.run(["docker", "service", "scale", f"{service_name}=1"], check=False)
                meta["disk_paths"] = new_paths
                meta["mergerfs_path"] = volume_path(service_name, volume_name)
                _write_meta(service_name, volume_name, meta)
                yield "[DONE]"
                return
            yield line
        # If generator exhausted without [DONE] — already yielded [ERROR]

    return StreamingResponse(_sse_bytes(gen()), media_type="text/event-stream")
