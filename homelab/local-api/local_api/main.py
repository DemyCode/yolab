import asyncio
import json
import os
import subprocess

import httpx
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import StreamingResponse

from local_api.apps import router as apps_router

REPO_PATH = os.environ.get("YOLAB_REPO_PATH", "/etc/nixos")
PLATFORM = os.environ.get("YOLAB_PLATFORM", "nixos")
FLAKE_TARGET = os.environ.get("YOLAB_FLAKE_TARGET", "yolab")
NODE_AGENT_PORT = 3002


def get_update_commands() -> list[list[str]]:
    rebuild = (
        ["darwin-rebuild", "switch", "--flake", f"{REPO_PATH}#{FLAKE_TARGET}"]
        if PLATFORM == "darwin"
        else ["nixos-rebuild", "switch", "--flake", f"{REPO_PATH}#{FLAKE_TARGET}"]
    )
    return [["git", "-C", REPO_PATH, "pull"], rebuild]


def _cluster_node_ips() -> list[str]:
    try:
        result = subprocess.run(
            ["kubectl", "get", "nodes", "-o", "json"],
            capture_output=True, text=True, check=True,
        )
        data = json.loads(result.stdout)
        ips = []
        for item in data.get("items", []):
            for addr in item.get("status", {}).get("addresses", []):
                if addr["type"] == "InternalIP":
                    ips.append(addr["address"])
        return ips
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        return []


async def _fetch_peer(client: httpx.AsyncClient, ip: str, path: str):
    host = f"[{ip}]" if ":" in ip else ip
    try:
        resp = await client.get(f"http://{host}:{NODE_AGENT_PORT}{path}", timeout=5)
        resp.raise_for_status()
        return resp.json()
    except Exception:
        return None


async def _fan_out(path: str, tag_hostname: bool = False) -> list:
    ips = _cluster_node_ips() or ["127.0.0.1"]
    async with httpx.AsyncClient() as client:
        node_infos, results = await asyncio.gather(
            asyncio.gather(*[_fetch_peer(client, ip, "/info") for ip in ips]),
            asyncio.gather(*[_fetch_peer(client, ip, path) for ip in ips]),
        )
    merged = []
    for info, r in zip(node_infos, results):
        hostname = (info or {}).get("hostname") if tag_hostname else None
        if isinstance(r, list):
            for item in r:
                if hostname and isinstance(item, dict):
                    item["node_hostname"] = hostname
                merged.append(item)
        elif r is not None:
            if hostname and isinstance(r, dict):
                r["node_hostname"] = hostname
            merged.append(r)
    return merged


app = FastAPI()
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)
app.include_router(apps_router)


@app.get("/api/health")
def health():
    return {"status": "ok"}


@app.get("/api/status")
def status():
    try:
        result = subprocess.run(
            ["git", "-C", REPO_PATH, "log", "-1", "--format=%H|||%s|||%ci"],
            capture_output=True, text=True, check=True,
        )
        parts = result.stdout.strip().split("|||")
        return {
            "commit_hash": parts[0] if len(parts) > 0 else "",
            "commit_message": parts[1] if len(parts) > 1 else "",
            "commit_date": parts[2] if len(parts) > 2 else "",
            "platform": PLATFORM,
            "flake_target": FLAKE_TARGET,
        }
    except Exception as e:
        return {"error": str(e), "platform": PLATFORM}


@app.post("/api/update")
async def update():
    async def stream():
        for cmd in get_update_commands():
            yield f"data: $ {' '.join(cmd)}\n\n"
            process = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
            async for line in process.stdout:
                text = line.decode().rstrip()
                if text:
                    yield f"data: {text}\n\n"
            await process.wait()
            if process.returncode != 0:
                yield f"data: [ERROR] exited with code {process.returncode}\n\n"
                return
        yield "data: [DONE]\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")


@app.get("/api/nodes")
async def get_nodes():
    ips = _cluster_node_ips() or ["127.0.0.1"]
    async with httpx.AsyncClient() as client:
        results = await asyncio.gather(*[_fetch_peer(client, ip, "/info") for ip in ips])
    nodes = []
    for ip, info in zip(ips, results):
        if info:
            info["agent_ip"] = ip
            nodes.append(info)
        elif ip == "127.0.0.1":
            # Node-agent not reachable — return minimal local info so the node always appears.
            import socket as _socket
            nodes.append({
                "node_id": "",
                "hostname": _socket.gethostname(),
                "platform": PLATFORM,
                "k3s_role": "server",
                "wg_ipv6": "",
                "agent_ip": ip,
            })
    return nodes


@app.get("/api/disks")
async def get_disks():
    return await _fan_out("/disks", tag_hostname=True)


@app.get("/api/volumes")
async def get_volumes():
    return await _fan_out("/volumes")


@app.get("/api/cluster/status")
async def get_cluster_status():
    try:
        result = subprocess.run(
            ["kubectl", "get", "nodes", "-o", "json"],
            capture_output=True, text=True, check=True,
        )
        data = json.loads(result.stdout)
        items = data.get("items", [])
        ready = sum(
            1 for n in items
            if any(c["type"] == "Ready" and c["status"] == "True"
                   for c in n.get("status", {}).get("conditions", []))
        )
        return {"total": len(items), "ready": ready}
    except Exception as e:
        return {"error": str(e)}


def run():
    import uvicorn
    uvicorn.run(app, host="127.0.0.1", port=3001)
