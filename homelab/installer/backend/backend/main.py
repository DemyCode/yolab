#!/usr/bin/env python3
import subprocess

from fastapi import FastAPI, HTTPException
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel

from backend.functions import get_status, install, scan_wifi, wifi_connect

app = FastAPI(title="YoLab Installer API")

app.add_middleware(
    CORSMiddleware,  # ty: ignore[invalid-argument-type]
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


class WifiConnectRequest(BaseModel):
    ssid: str
    password: str = ""


class InstallRequest(BaseModel):
    disk: str
    hostname: str
    timezone: str
    root_ssh_key: str
    git_remote: str


@app.get("/api/status")
async def api_get_status():
    return get_status()


@app.get("/api/wifi/scan")
async def api_scan_wifi():
    return scan_wifi()


@app.post("/api/wifi/connect")
async def api_wifi_connect(request: WifiConnectRequest):
    try:
        return wifi_connect(request.ssid, request.password)
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/api/install")
async def api_install(request: InstallRequest):
    try:
        return install(
            request.disk,
            request.hostname,
            request.timezone,
            request.root_ssh_key,
            request.git_remote,
        )
    except subprocess.CalledProcessError as e:
        raise HTTPException(status_code=500, detail=f"Installation failed: {str(e)}")
    except Exception as e:
        raise HTTPException(status_code=400, detail=str(e))


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="0.0.0.0", port=8000)
