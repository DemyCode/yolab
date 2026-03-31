from fastapi import APIRouter

from local_api.routes import health, info, nfs, k3s

app_router = APIRouter()
app_router.include_router(health.router, prefix="/api", tags=["info"])
app_router.include_router(info.router, prefix="/api", tags=["info"])
app_router.include_router(nfs.router, prefix="/api", tags=["nfs"])
app_router.include_router(k3s.router, prefix="/api", tags=["k3s"])
