import asyncio
import json
import subprocess
from pathlib import Path

import httpx
import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import StreamingResponse

from local_api.settings import settings

REBUILD_LOG = Path("/var/log/yolab-rebuild.log")
REBUILD_PID = Path("/run/yolab-rebuild.pid")

app = FastAPI()
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)


def _git(*args: str) -> str:
    return subprocess.check_output(
        ["git", *args], cwd=settings.yolab_repo_path, text=True
    ).strip()


def _lsblk() -> list[dict]:
    out = subprocess.check_output(
        ["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL"],
        text=True,
    )
    return json.loads(out)["blockdevices"]


def _df_used(mountpoint: str) -> int:
    out = subprocess.check_output(
        ["df", "-B1", "--output=used", mountpoint],
        text=True,
    )
    return int(out.strip().splitlines()[1].strip())


def _collect_mountpoints(device: dict) -> list[str]:
    mounts = []
    mp = device.get("mountpoint")
    if mp and mp != "[SWAP]":
        mounts.append(mp)
    for child in device.get("children") or []:
        mounts.extend(_collect_mountpoints(child))
    return mounts


def _disk_entry(device: dict) -> dict:
    mounts = _collect_mountpoints(device)
    used = 0
    for m in mounts:
        try:
            used += _df_used(m)
        except Exception:
            pass
    size = device.get("size") or 0
    return {
        "name": device["name"],
        "model": (device.get("model") or "").strip(),
        "size_bytes": int(size),
        "used_bytes": used,
        "mountpoints": mounts,
        "host": settings.yolab_node_ipv6,
    }


def _kubectl_node_ips() -> list[str]:
    out = subprocess.check_output(["kubectl", "get", "nodes", "-o", "json"], text=True)
    data = json.loads(out)
    ips = []
    for item in data["items"]:
        for addr in item["status"]["addresses"]:
            if addr["type"] == "InternalIP":
                ips.append(addr["address"])
                break
    return ips


@app.get("/api/status")
async def status():
    try:
        return {
            "commit_hash": _git("rev-parse", "HEAD"),
            "commit_message": _git("log", "-1", "--pretty=%s"),
            "commit_date": _git("log", "-1", "--pretty=%cI"),
            "platform": settings.yolab_platform,
            "flake_target": settings.yolab_flake_target,
        }
    except Exception as e:
        return {
            "commit_hash": "",
            "commit_message": "",
            "commit_date": "",
            "platform": settings.yolab_platform,
            "flake_target": settings.yolab_flake_target,
            "error": str(e),
        }


@app.post("/api/update")
async def update():
    async def stream():
        yield f"data: $ git -C {settings.yolab_repo_path} pull\n\n"
        try:
            proc = subprocess.Popen(
                ["git", "-C", settings.yolab_repo_path, "pull"],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            for line in proc.stdout:
                yield f"data: {line.rstrip()}\n\n"
            proc.wait()
            if proc.returncode != 0:
                yield f"data: [ERROR] git pull failed (exit {proc.returncode})\n\n"
                return
        except Exception as e:
            yield f"data: [ERROR] {e}\n\n"
            return

        flake = f"path:{settings.yolab_repo_path}#{settings.yolab_flake_target}"
        yield f"data: $ nixos-rebuild switch --flake {flake} --verbose --print-build-logs\n\n"
        yield "data: [INFO] nixos-rebuild launched — service will restart shortly\n\n"

        REBUILD_LOG.parent.mkdir(parents=True, exist_ok=True)
        log_file = open(REBUILD_LOG, "w")
        proc = subprocess.Popen(
            [
                "nixos-rebuild",
                "switch",
                "--flake",
                flake,
                "--verbose",
                "--print-build-logs",
            ],
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=log_file,
            start_new_session=True,
        )
        log_file.close()
        REBUILD_PID.write_text(str(proc.pid))

    return StreamingResponse(stream(), media_type="text/event-stream")


@app.get("/api/rebuild-log")
async def rebuild_log():
    running = False
    if REBUILD_PID.exists():
        try:
            pid = int(REBUILD_PID.read_text().strip())
            running = Path(f"/proc/{pid}").exists()
        except Exception:
            pass
    log = REBUILD_LOG.read_text().splitlines() if REBUILD_LOG.exists() else []
    return {"running": running, "log": log}


@app.get("/api/disks/local")
async def disks_local():
    devices = await asyncio.to_thread(_lsblk)
    return [_disk_entry(d) for d in devices if d.get("type") == "disk"]


@app.get("/api/disks")
async def disks():
    node_ips = await asyncio.to_thread(_kubectl_node_ips)
    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[client.get(f"http://[{ip}]:{settings.port}/api/disks/local") for ip in node_ips],
            return_exceptions=True,
        )
    all_disks = []
    for r in results:
        if isinstance(r, Exception):
            continue
        if r.status_code == 200:
            all_disks.extend(r.json())
    return all_disks


def run():
    uvicorn.run(app, host="0.0.0.0", port=settings.port)
