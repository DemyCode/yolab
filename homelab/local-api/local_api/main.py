import asyncio
from contextlib import asynccontextmanager

import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from local_api.auth import AuthMiddleware
from local_api.routers import apps, disks, nodes, rebuild, status, terminal, update
from local_api import auth
from local_api.settings import settings


@asynccontextmanager
async def lifespan(app: FastAPI):
    task = asyncio.create_task(disks.auto_enable_loop())
    yield
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


app = FastAPI(lifespan=lifespan)
app.add_middleware(AuthMiddleware)
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
    allow_credentials=True,
)

app.include_router(auth.router)
app.include_router(status.router)
app.include_router(update.router)
app.include_router(rebuild.router)
app.include_router(disks.router)
app.include_router(nodes.router)
app.include_router(apps.router)
app.include_router(terminal.router)


def run():
    uvicorn.run(app, host="::", port=settings.port)
