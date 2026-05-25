import uvicorn
from fastapi import APIRouter, FastAPI
from fastapi.middleware.cors import CORSMiddleware

from local_api.auth import AuthMiddleware
from local_api.routers import apps, disks, nodes, rebuild, status, terminal, update
from local_api import auth
from local_api.settings import settings


api = APIRouter(prefix="/api")
api.include_router(auth.router)
api.include_router(status.router)
api.include_router(update.router)
api.include_router(rebuild.router)
api.include_router(disks.router)
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


def run():
    uvicorn.run(app, host="::", port=settings.port)
