import json
import os
import subprocess
from pathlib import Path

import httpx
from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles


def dict_to_toml(obj, indent=""):
    lines = []
    simple = {}
    tables = {}
    arrays = []

    for key, value in obj.items():
        if isinstance(value, list) and value and isinstance(value[0], dict):
            arrays.append((key, value))
        elif isinstance(value, dict):
            tables[key] = value
        else:
            simple[key] = value

    for key, value in simple.items():
        if isinstance(value, list):
            lines.append(f"{key} = {json.dumps(value)}")
        elif isinstance(value, str):
            lines.append(f'{key} = "{value}"')
        elif isinstance(value, bool):
            lines.append(f"{key} = {str(value).lower()}")
        else:
            lines.append(f"{key} = {value}")

    for key, value in tables.items():
        lines.append(f"\n[{key}]")
        lines.append(dict_to_toml(value, indent + "  "))

    for key, items in arrays:
        for item in items:
            lines.append(f"\n[[{key}]]")
            lines.append(dict_to_toml(item, indent + "  "))

    return "\n".join(lines)


def toml_to_dict(text):
    lines = text.split("\n")
    result = {}
    current = result
    current_path = []
    current_array = None

    for line in lines:
        line = line.strip()
        if not line or line.startswith("#"):
            continue

        if line.startswith("[[") and line.endswith("]]"):
            array_name = line[2:-2]
            if array_name not in result:
                result[array_name] = []
            current_array = {}
            result[array_name].append(current_array)
            current = current_array
        elif line.startswith("[") and line.endswith("]"):
            section = line[1:-1]
            current_path = section.split(".")
            current = result
            current_array = None

            for part in current_path:
                if part not in current:
                    current[part] = {}
                current = current[part]
        elif "=" in line:
            key, _, value = line.partition("=")
            key = key.strip()
            value = value.strip()

            try:
                current[key] = json.loads(value)
            except (json.JSONDecodeError, ValueError):
                if value.lower() in ("true", "false"):
                    current[key] = value.lower() == "true"
                else:
                    current[key] = value.strip("\"'")

    return result


class Settings:
    platform_api_url: str
    config_path: str
    services_dir: str

    def __init__(self):
        self.platform_api_url = os.getenv("PLATFORM_API_URL", "http://localhost:5000")
        self.config_path = os.getenv("CONFIG_PATH", "/etc/yolab/config.toml")
        self.services_dir = os.getenv("SERVICES_DIR", "/var/lib/yolab/services")


app = FastAPI(title="YoLab Client UI")
settings = Settings()

CONFIG_PATH = Path(settings.config_path)
SERVICES_DIR = Path(settings.services_dir)
FRONTEND_DIR = Path(__file__).parent / "frontend" / "dist"

SERVICES_DIR.mkdir(parents=True, exist_ok=True)

if FRONTEND_DIR.exists():
    app.mount("/assets", StaticFiles(directory=FRONTEND_DIR / "assets"), name="assets")

    @app.get("/")
    async def root():
        return FileResponse(FRONTEND_DIR / "index.html")
else:

    @app.get("/")
    async def root():
        return {
            "error": "Frontend not built. Run: cd frontend && npm install && npm run build"
        }


@app.get("/config")
async def get_config():
    if not CONFIG_PATH.exists():
        raise HTTPException(status_code=404, detail="Config file not found")

    with open(CONFIG_PATH) as f:
        config = toml_to_dict(f.read())

    return config


@app.post("/config")
async def update_config(config: dict):
    CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)

    with open(CONFIG_PATH, "w") as f:
        f.write(dict_to_toml(config))

    return {"status": "success"}


@app.get("/services/available")
async def list_available_services():
    async with httpx.AsyncClient() as client:
        response = await client.get(f"{settings.platform_api_url}/api/templates")
        response.raise_for_status()
        return response.json()


@app.get("/services/downloaded")
async def list_downloaded_services():
    if not SERVICES_DIR.exists():
        return []

    services = []
    for service_dir in SERVICES_DIR.iterdir():
        if service_dir.is_dir():
            has_compose = (service_dir / "docker-compose.yml").exists()
            has_caddy = (service_dir / "Caddyfile").exists()
            services.append(
                {
                    "name": service_dir.name,
                    "has_compose": has_compose,
                    "has_caddy": has_caddy,
                }
            )

    return services


@app.post("/services/download/{service_name}")
async def download_service(service_name: str):
    async with httpx.AsyncClient() as client:
        response = await client.get(
            f"{settings.platform_api_url}/api/templates/{service_name}"
        )
        response.raise_for_status()
        data = response.json()

    service_dir = SERVICES_DIR / service_name
    service_dir.mkdir(parents=True, exist_ok=True)

    (service_dir / "docker-compose.yml").write_text(data["docker_compose"])
    (service_dir / "Caddyfile").write_text(data["caddyfile"])

    return {"status": "success", "service": service_name}


@app.post("/services/delete/{service_name}")
async def delete_service(service_name: str):
    service_dir = SERVICES_DIR / service_name

    if not service_dir.exists():
        raise HTTPException(status_code=404, detail="Service not found")

    import shutil

    shutil.rmtree(service_dir)

    return {"status": "success", "service": service_name}


@app.post("/rebuild")
async def rebuild_system():
    try:
        result = subprocess.run(
            ["nixos-rebuild", "switch", "--flake", "/etc/nixos#yolab"],
            capture_output=True,
            text=True,
            timeout=300,
        )

        return {
            "status": "success" if result.returncode == 0 else "error",
            "stdout": result.stdout,
            "stderr": result.stderr,
            "returncode": result.returncode,
        }
    except subprocess.TimeoutExpired:
        raise HTTPException(status_code=408, detail="Rebuild timeout")
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.get("/health")
async def health():
    return {"status": "healthy"}


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="0.0.0.0", port=8080)
