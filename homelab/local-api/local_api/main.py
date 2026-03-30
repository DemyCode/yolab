import asyncio
import json
import os
import shlex
import subprocess

import httpx
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import StreamingResponse

from local_api.apps import router as apps_router

REPO_PATH = os.environ.get("YOLAB_REPO_PATH", "/etc/nixos")
PLATFORM = os.environ.get("YOLAB_PLATFORM", "nixos")
FLAKE_TARGET = os.environ.get("YOLAB_FLAKE_TARGET", "yolab")

_KUBECTL_ENV = {
    **os.environ,
    "KUBECONFIG": os.environ.get("KUBECONFIG", "/etc/rancher/k3s/k3s.yaml"),
}
NODE_AGENT_PORT = 3002

UPDATE_LOG = "/tmp/yolab-update.log"
UPDATE_SCRIPT = "/tmp/yolab-update.sh"
UPDATE_UNIT = "yolab-update.service"
UPDATE_PID_FILE = "/tmp/yolab-update.pid"


def get_update_commands() -> list[list[str]]:
    if PLATFORM not in ("nixos", "darwin"):
        raise ValueError(f"Unsupported platform: {PLATFORM}")
    git_cmd = [["git", "-C", REPO_PATH, "pull"]]
    nix_store_verify = [["nix-store", "--verify", "--check-contents", "--repair"]]
    switch_cmd = [
        "switch",
        "--flake",
        f"path:{REPO_PATH}#{FLAKE_TARGET}",
        "--print-build-logs",
        "--verbose",
        "--repair",
        "--log-format",
        "raw",
    ]
    if PLATFORM == "nixos":
        return git_cmd + nix_store_verify + [["nixos-rebuild"] + switch_cmd] + [["reboot"]]
    elif PLATFORM == "darwin":
        return git_cmd + nix_store_verify + [["darwin-rebuild"] + switch_cmd]
    raise ValueError(f"Unsupported platform: {PLATFORM}")


def _build_update_script() -> str:
    lines = ["#!/bin/sh", f"exec > {UPDATE_LOG} 2>&1"]
    if PLATFORM == "darwin":
        lines.append(f'echo $$ > {UPDATE_PID_FILE}')
    for cmd in get_update_commands():
        display = " ".join(cmd)
        quoted = shlex.join(cmd)
        lines.append(f'echo "$ {display}"')
        lines.append(quoted)
        lines.append(
            f'rc=$?; if [ $rc -ne 0 ]; then echo "[ERROR] {cmd[0]} exited with code $rc"; exit 1; fi'
        )
    lines.append('echo "[DONE]"')
    if PLATFORM == "darwin":
        lines.append(f'rm -f {UPDATE_PID_FILE}')
    return "\n".join(lines) + "\n"


def _launch_update() -> None:
    if PLATFORM in ("nixos", "wsl"):
        subprocess.run(
            [
                "systemd-run",
                f"--unit={UPDATE_UNIT.removesuffix('.service')}",
                "--description=YoLab homelab update",
                "/bin/sh",
                UPDATE_SCRIPT,
            ],
            check=True,
            capture_output=True,
        )
    else:
        proc = subprocess.Popen(
            ["/bin/sh", UPDATE_SCRIPT],
            start_new_session=True,
            close_fds=True,
        )
        with open(UPDATE_PID_FILE, "w") as f:
            f.write(str(proc.pid))


def _is_update_running() -> bool:
    if PLATFORM in ("nixos", "wsl"):
        result = subprocess.run(
            ["systemctl", "is-active", UPDATE_UNIT],
            capture_output=True,
            text=True,
        )
        return result.stdout.strip() in ("active", "activating")
    else:
        if not os.path.exists(UPDATE_PID_FILE):
            return False
        try:
            with open(UPDATE_PID_FILE) as f:
                pid = int(f.read().strip())
            os.kill(pid, 0)
            return True
        except (ValueError, ProcessLookupError, PermissionError, OSError):
            return False


def _stop_update() -> None:
    if PLATFORM in ("nixos", "wsl"):
        subprocess.run(["systemctl", "stop", UPDATE_UNIT], capture_output=True)
    else:
        if os.path.exists(UPDATE_PID_FILE):
            try:
                with open(UPDATE_PID_FILE) as f:
                    pid = int(f.read().strip())
                os.kill(pid, 15)
            except Exception:
                pass
            os.unlink(UPDATE_PID_FILE)


def _cluster_node_ips() -> list[str]:
    try:
        result = subprocess.run(
            ["kubectl", "get", "nodes", "-o", "json"],
            capture_output=True,
            text=True,
            check=True,
            env=_KUBECTL_ENV,
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
        wg_ipv6 = (info or {}).get("wg_ipv6", "") if tag_hostname else None
        if isinstance(r, list):
            for item in r:
                if hostname and isinstance(item, dict):
                    item["node_hostname"] = hostname
                    item["node_wg_ipv6"] = wg_ipv6
                merged.append(item)
        elif r is not None:
            if hostname and isinstance(r, dict):
                r["node_hostname"] = hostname
                r["node_wg_ipv6"] = wg_ipv6
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
            capture_output=True,
            text=True,
            check=True,
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


@app.get("/api/update/status")
def update_status():
    running = _is_update_running()
    log: list[str] = []
    try:
        with open(UPDATE_LOG) as f:
            log = f.read().splitlines()
    except FileNotFoundError:
        pass
    return {"running": running, "log": log}


@app.post("/api/update")
async def update():
    async def stream():
        _stop_update()

        with open(UPDATE_SCRIPT, "w") as f:
            f.write(_build_update_script())
        os.chmod(UPDATE_SCRIPT, 0o755)

        open(UPDATE_LOG, "w").close()

        try:
            _launch_update()
        except subprocess.CalledProcessError as e:
            yield f"data: [ERROR] failed to start update: {e.stderr.decode()}\n\n"
            return

        yield "data: Update started\n\n"

        pos = 0
        while True:
            try:
                with open(UPDATE_LOG) as f:
                    f.seek(pos)
                    chunk = f.read()
            except FileNotFoundError:
                await asyncio.sleep(0.1)
                continue

            if chunk:
                pos += len(chunk)
                for line in chunk.splitlines():
                    if line:
                        yield f"data: {line}\n\n"
                if "[DONE]" in chunk or "[ERROR]" in chunk:
                    return

            if not _is_update_running():
                try:
                    with open(UPDATE_LOG) as f:
                        f.seek(pos)
                        remainder = f.read()
                    for line in remainder.splitlines():
                        if line:
                            yield f"data: {line}\n\n"
                except FileNotFoundError:
                    pass
                return

            await asyncio.sleep(0.2)

    return StreamingResponse(stream(), media_type="text/event-stream")


@app.get("/api/nodes")
async def get_nodes():
    ips = _cluster_node_ips() or ["127.0.0.1"]
    async with httpx.AsyncClient() as client:
        results = await asyncio.gather(
            *[_fetch_peer(client, ip, "/info") for ip in ips]
        )
    nodes = []
    for ip, info in zip(ips, results):
        if info:
            info["agent_ip"] = ip
            nodes.append(info)
        elif ip == "127.0.0.1":
            import socket as _socket
            nodes.append(
                {
                    "node_id": "",
                    "hostname": _socket.gethostname(),
                    "platform": PLATFORM,
                    "k3s_role": "server",
                    "wg_ipv6": "",
                    "agent_ip": ip,
                }
            )
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
            capture_output=True,
            text=True,
            check=True,
            env=_KUBECTL_ENV,
        )
        data = json.loads(result.stdout)
        items = data.get("items", [])
        ready = sum(
            1
            for n in items
            if any(
                c["type"] == "Ready" and c["status"] == "True"
                for c in n.get("status", {}).get("conditions", [])
            )
        )
        return {"total": len(items), "ready": ready}
    except Exception as e:
        return {"error": str(e)}


def run():
    import uvicorn
    uvicorn.run(app, host="127.0.0.1", port=3001)
