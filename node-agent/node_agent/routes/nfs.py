from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from node_agent import config
from node_agent import nfs

router = APIRouter(tags=["nfs"])


def _check_platform():
    if config.YOLAB_PLATFORM == "wsl":
        raise HTTPException(status_code=501, detail="NFS not supported on WSL")


class ExportRequest(BaseModel):
    disk_id: str
    mount_path: str


class MountRequest(BaseModel):
    disk_id: str
    remote_ipv6: str
    remote_path: str


@router.post("/nfs/export")
def create_export(req: ExportRequest):
    _check_platform()
    nfs.export_disk(req.disk_id, req.mount_path)
    return {"disk_id": req.disk_id, "exported": True}


@router.delete("/nfs/export/{disk_id}")
def remove_export(disk_id: str):
    _check_platform()
    nfs.unexport_disk(disk_id)
    return {"disk_id": disk_id, "exported": False}


@router.post("/nfs/mount")
def mount_remote(req: MountRequest):
    _check_platform()
    local_path = nfs.mount_remote(req.disk_id, req.remote_ipv6, req.remote_path)
    return {"disk_id": req.disk_id, "local_path": local_path}


@router.delete("/nfs/mount/{disk_id}")
def umount_remote(disk_id: str):
    _check_platform()
    nfs.umount_remote(disk_id)
    return {"disk_id": disk_id, "mounted": False}
