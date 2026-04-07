import asyncio
from contextlib import asynccontextmanager

import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from local_api.routers import apps, disks, nodes, rebuild, status, update
from local_api.settings import settings


@asynccontextmanager
async def lifespan(app: FastAPI):
    await asyncio.to_thread(disks.auto_enable_all_storage)
    yield


app = FastAPI(lifespan=lifespan)
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)

app.include_router(status.router)
app.include_router(update.router)
app.include_router(rebuild.router)
app.include_router(disks.router)
app.include_router(nodes.router)
app.include_router(apps.router)


def run():
    uvicorn.run(app, host="::", port=settings.port)
