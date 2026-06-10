import asyncio

import uvicorn
from fastapi import APIRouter, FastAPI
from fastapi.middleware.cors import CORSMiddleware

from local_api import auth
from local_api.auth import AuthMiddleware
from local_api.routers import apps, ceph, disks, nodes, rebuild, status, terminal, update
from local_api.settings import settings

api = APIRouter(prefix="/api")
api.include_router(auth.router)
api.include_router(status.router)
api.include_router(update.router)
api.include_router(rebuild.router)
api.include_router(disks.router)
api.include_router(ceph.router)
api.include_router(nodes.router)
api.include_router(apps.router)
api.include_router(terminal.router)

app = FastAPI()
app.add_middleware(AuthMiddleware)
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
    allow_credentials=True,
)
app.include_router(api)


def _is_primary_node() -> bool:
    return settings.k3s_server_dir.is_dir()


async def _activation_loop() -> None:
    while True:
        await asyncio.sleep(60)
        try:
            from local_api.routers.disks import _reconcile_storage
            await _reconcile_storage()
        except Exception:
            pass


@app.on_event("startup")
async def _startup() -> None:
    if _is_primary_node():
        asyncio.create_task(_activation_loop())


def run():
    uvicorn.run(app, host="::", port=settings.port)
