import asyncio
import logging
import queue
import threading

import httpx
import uvicorn
from fastapi import FastAPI, HTTPException
from fastapi.responses import JSONResponse, StreamingResponse

from installer.install_flow import build_install_config, install_system
from installer.password import hash_password
from installer.ssh_keygen import generate_ssh_keypair
from installer.system import detect_disks

logging.basicConfig(level=logging.DEBUG)

app = FastAPI()

_state: dict = {}
_log_q: queue.Queue = queue.Queue()
_install_started = threading.Event()

# ── Internal state ─────────────────────────────────────────────────────────────


@app.post("/api/tunnel")
async def api_set_tunnel(body: dict) -> dict:
    _state["tunnel"] = body
    return {"ok": True}


# ── Cluster join helpers ───────────────────────────────────────────────────────


@app.post("/api/join-info")
async def api_join_info(body: dict) -> dict:
    """Authenticate against an existing yolab node and fetch its k3s join info.

    Body: {"url": "https://yolab.example.com", "password": "..."}
    Returns: {"k3s_token": "...", "server_addr": "https://[fd00:cafe::1]:6443"}
    """
    url = body.get("url", "").rstrip("/")
    password = body.get("password", "")
    if not url:
        raise HTTPException(status_code=422, detail="url is required")

    login_resp = httpx.post(
        f"{url}/api/login",
        json={"password": password},
        timeout=10,
        follow_redirects=True,
    )
    if login_resp.status_code != 200:
        raise HTTPException(
            status_code=401, detail="Authentication failed on existing node"
        )

    session_cookie = login_resp.cookies.get("yolab_session")
    if not session_cookie:
        raise HTTPException(
            status_code=401, detail="No session cookie received from existing node"
        )

    info_resp = httpx.get(
        f"{url}/api/cluster/join-info",
        cookies={"yolab_session": session_cookie},
        timeout=10,
    )
    if info_resp.status_code != 200:
        raise HTTPException(
            status_code=info_resp.status_code,
            detail="Failed to fetch join info from existing node",
        )
    return info_resp.json()


# ── Disk & SSH helpers ─────────────────────────────────────────────────────────


@app.get("/api/disks")
async def api_disks() -> list:
    try:
        return detect_disks()
    except Exception as e:
        logging.exception("detect_disks failed")
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/api/generate-ssh-key")
async def api_generate_ssh_key() -> dict:
    private_key, public_key = generate_ssh_keypair()
    return {"private_key": private_key, "public_key": public_key}


# ── Install ────────────────────────────────────────────────────────────────────


@app.post("/api/install")
async def api_install(body: dict) -> JSONResponse:
    if _install_started.is_set():
        return JSONResponse(
            status_code=409, content={"detail": "Installation already in progress"}
        )

    disk = body.get("disk", "")
    hostname = body.get("hostname", "homelab")
    timezone = body.get("timezone", "UTC")
    password = body.get("password", "")
    ssh_key = body.get("ssh_key", "")
    server_addr = body.get("server_addr", "")
    k3s_token = body.get("k3s_token") or None

    if not disk:
        return JSONResponse(status_code=422, content={"detail": "Disk is required"})
    if len(password) < 8:
        return JSONResponse(
            status_code=422,
            content={"detail": "Password must be at least 8 characters"},
        )

    config = build_install_config(
        disk=disk,
        hostname=hostname,
        timezone=timezone,
        root_ssh_key=ssh_key,
        homelab_password_hash=hash_password(password),
        tunnel=_state.get("tunnel"),
        server_addr=server_addr,
        k3s_token=k3s_token,
    )

    _install_started.set()
    threading.Thread(target=_run_install, args=(config,), daemon=True).start()
    return JSONResponse(content={"ok": True})


@app.get("/api/progress")
async def api_progress() -> StreamingResponse:
    async def generate():
        loop = asyncio.get_running_loop()
        while True:
            try:
                line = await loop.run_in_executor(None, lambda: _log_q.get(timeout=120))
            except queue.Empty:
                yield "data: \n\n"
                continue
            yield f"data: {line}\n\n"
            if line in ("__DONE__", "__ERROR__"):
                break

    return StreamingResponse(
        generate(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache"},
    )


# ── Install runner ─────────────────────────────────────────────────────────────


def _run_install(config: dict) -> None:
    try:
        install_system(config, log=_log_q.put)
        _log_q.put("__DONE__")
    except Exception as e:
        _log_q.put(f"FATAL: {e}")
        _log_q.put("__ERROR__")


# ── Entry point ────────────────────────────────────────────────────────────────


def run(port: int = 8080) -> None:
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="debug")
