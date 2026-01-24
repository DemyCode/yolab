#!/usr/bin/env python3
import subprocess
import json
import os
from pathlib import Path
from fastapi import FastAPI, HTTPException
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles
from fastapi.responses import FileResponse
from pydantic import BaseModel

app = FastAPI()

app.add_middleware(
    CORSMiddleware,
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

def test_internet():
    try:
        result = subprocess.run(
            ["ping", "-c", "1", "-W", "2", "1.1.1.1"],
            capture_output=True,
            timeout=3,
        )
        return result.returncode == 0
    except:
        return False

def scan_wifi_networks():
    try:
        subprocess.run(["nmcli", "device", "wifi", "rescan"], capture_output=True, timeout=10)
        result = subprocess.run(
            ["nmcli", "-t", "-f", "SSID,SIGNAL,SECURITY", "device", "wifi", "list"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        networks = []
        for line in result.stdout.strip().split("\n"):
            if line:
                parts = line.split(":")
                if len(parts) >= 3 and parts[0]:
                    networks.append({
                        "ssid": parts[0],
                        "signal": parts[1],
                        "security": parts[2],
                    })
        return sorted(networks, key=lambda x: int(x["signal"]) if x["signal"] else 0, reverse=True)
    except:
        return []

def connect_wifi(ssid, password):
    try:
        if password:
            result = subprocess.run(
                ["nmcli", "device", "wifi", "connect", ssid, "password", password],
                capture_output=True,
                text=True,
                timeout=30,
            )
        else:
            result = subprocess.run(
                ["nmcli", "device", "wifi", "connect", ssid],
                capture_output=True,
                text=True,
                timeout=30,
            )
        return result.returncode == 0
    except:
        return False

def get_wifi_config():
    try:
        result = subprocess.run(
            ["nmcli", "-t", "-f", "NAME,TYPE", "connection", "show", "--active"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        for line in result.stdout.strip().split("\n"):
            if "802-11-wireless" in line:
                ssid = line.split(":")[0]
                psk_result = subprocess.run(
                    ["nmcli", "-s", "-g", "802-11-wireless-security.psk", "connection", "show", ssid],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                return {"ssid": ssid, "psk": psk_result.stdout.strip()}
        return None
    except:
        return None

def detect_disks():
    result = subprocess.run(
        ["lsblk", "-J", "-o", "NAME,SIZE,TYPE,MOUNTPOINT"],
        capture_output=True,
        text=True,
    )
    data = json.loads(result.stdout)
    disks = []
    for device in data.get("blockdevices", []):
        if device.get("type") == "disk":
            disks.append({
                "name": f"/dev/{device['name']}",
                "size": device["size"],
                "mounted": bool(device.get("mountpoint")),
            })
    return disks

def detect_ram_size():
    try:
        with open("/proc/meminfo", "r") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    kb = int(line.split()[1])
                    gb = kb // (1024 * 1024)
                    return min(gb, 32)
    except:
        return 8
    return 8

def generate_config_toml(disk, hostname, timezone, root_ssh_key, swap_size, git_remote, wifi_config):
    wifi_section = ""
    if wifi_config:
        wifi_section = f'''
[wifi]
ssid = "{wifi_config['ssid']}"
psk = "{wifi_config['psk']}"
'''

    return f'''[homelab]
hostname = "{hostname}"
timezone = "{timezone}"
locale = "en_US.UTF-8"
ssh_port = 22
root_ssh_key = "{root_ssh_key}"
git_remote = "{git_remote}"
allowed_ssh_keys = []

[disk]
device = "{disk}"
esp_size = "500M"
swap_size = "{swap_size}G"
{wifi_section}
[client_ui]
enabled = true
port = 8080
platform_api_url = ""

[docker]
enabled = false
compose_url = ""

[frpc]
enabled = false
server_addr = ""
server_port = 7000
account_token = ""
'''

def run_installation(disk, hostname, timezone, root_ssh_key, git_remote):
    install_dir = Path("/mnt/installer")
    install_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        ["git", "clone", git_remote, str(install_dir)],
        check=True,
        capture_output=True,
    )

    swap_size = detect_ram_size()
    wifi_config = get_wifi_config()

    config_toml = install_dir / "config.toml"
    config_toml.write_text(
        generate_config_toml(disk, hostname, timezone, root_ssh_key, swap_size, git_remote, wifi_config)
    )

    subprocess.run(
        ["nixos-generate-config", "--no-filesystems", "--dir", str(install_dir)],
        check=True,
        capture_output=True,
    )

    subprocess.run(
        [
            "nix",
            "--extra-experimental-features",
            "nix-command flakes",
            "run",
            "github:nix-community/disko#disko-install",
            "--",
            "--flake",
            f"{install_dir}#yolab",
            "--disk",
            "disk1",
            disk,
        ],
        check=True,
        capture_output=True,
    )

    nixos_dir = Path("/mnt/etc/nixos")
    nixos_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        ["cp", "-rT", str(install_dir), str(nixos_dir)],
        check=True,
        capture_output=True,
    )

@app.get("/api/status")
async def get_status():
    return {
        "internet": test_internet(),
        "disks": detect_disks(),
    }

@app.get("/api/wifi/scan")
async def scan_wifi():
    networks = scan_wifi_networks()
    return {"networks": networks}

@app.post("/api/wifi/connect")
async def wifi_connect(request: WifiConnectRequest):
    success = connect_wifi(request.ssid, request.password)
    if not success:
        raise HTTPException(status_code=500, detail="Failed to connect to WiFi")
    return {"success": True, "message": f"Connected to {request.ssid}"}

@app.post("/api/install")
async def install(request: InstallRequest):
    if not test_internet():
        raise HTTPException(status_code=400, detail="Internet connection required")

    try:
        run_installation(
            request.disk,
            request.hostname,
            request.timezone,
            request.root_ssh_key,
            request.git_remote
        )
        return {
            "success": True,
            "message": "Installation complete",
            "hostname": request.hostname,
            "disk": request.disk,
            "git_remote": request.git_remote,
        }
    except subprocess.CalledProcessError as e:
        raise HTTPException(status_code=500, detail=f"Installation failed: {str(e)}")

frontend_dir_env = os.environ.get("FRONTEND_DIR")
if frontend_dir_env:
    frontend_dir = Path(frontend_dir_env)
else:
    frontend_dir = Path(__file__).parent.parent / "frontend" / "dist"

if frontend_dir.exists():
    app.mount("/assets", StaticFiles(directory=str(frontend_dir / "assets")), name="assets")

    @app.get("/{full_path:path}")
    async def serve_frontend(full_path: str):
        file_path = frontend_dir / full_path
        if file_path.exists() and file_path.is_file():
            return FileResponse(file_path)
        return FileResponse(frontend_dir / "index.html")

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8000)
