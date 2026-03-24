import socket

from fastapi import APIRouter

from node_agent import config

router = APIRouter(tags=["info"])


@router.get("/health")
def health():
    return {"status": "ok"}


@router.get("/info")
def info():
    return {
        "node_id": config.NODE_ID,
        "hostname": socket.gethostname(),
        "platform": config.YOLAB_PLATFORM,
        "k3s_role": config.K3S_ROLE,
        "wg_ipv6": config.WG_IPV6,
    }
