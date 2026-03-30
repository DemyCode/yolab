import asyncio
import logging
import threading
from contextlib import asynccontextmanager

import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from node_agent.health import health_loop
from node_agent.routes import disks, info, k3s, nfs

logging.basicConfig(level=logging.INFO)


def _start_csi_server():
    try:
        from node_agent.csi.server import create_csi_server
        server = create_csi_server()
        server.start()
        server.wait_for_termination()
    except Exception as e:
        logging.getLogger("csi").error("CSI server failed: %s", e)


@asynccontextmanager
async def lifespan(app: FastAPI):
    csi_thread = threading.Thread(target=_start_csi_server, daemon=True)
    csi_thread.start()

    health_task = asyncio.create_task(health_loop())
    yield
    health_task.cancel()


app = FastAPI(title="YoLab Node Agent", lifespan=lifespan)
app.add_middleware(CORSMiddleware, allow_origins=["*"], allow_methods=["*"], allow_headers=["*"])

app.include_router(info.router)
app.include_router(disks.router)
app.include_router(nfs.router)
app.include_router(k3s.router)


def run():
    uvicorn.run(app, host="127.0.0.1", port=3002)
