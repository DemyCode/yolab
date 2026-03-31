from fastapi import APIRouter, HTTPException

from local_api import config
from local_api import k3s

router = APIRouter(tags=["k3s"])


def _check_platform():
    if config.PLATFORM == "wsl":
        raise HTTPException(status_code=501, detail="K3s not supported on WSL")


@router.get("/k3s/status")
def k3s_status():
    return k3s.k3s_status()


@router.get("/k3s/nodes")
def k3s_nodes():
    _check_platform()
    return k3s.list_nodes()
