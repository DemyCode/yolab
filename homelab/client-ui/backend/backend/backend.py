import subprocess
from pathlib import Path

import httpx
from fastapi import FastAPI, HTTPException
from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict

from backend.functions import (
    delete_service,
    download_service,
    list_available_services,
    list_downloaded_services,
    read_config,
    rebuild_system,
    validate_config,
    write_config,
)


class ClientUISettings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=str(Path(__file__).parent.parent / ".env"),
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
    )
    platform_api_url: str = Field(default="http://localhost:5000")
    config_path: str = Field(default="/etc/yolab/config.toml")
    services_dir: str = Field(default="/var/lib/yolab/services")
    flake_path: str = Field(default="/etc/nixos#yolab")
    port: int = Field(default=8080, ge=1, le=65535)


settings = ClientUISettings()

app = FastAPI(title="YoLab Client UI API")

CONFIG_PATH = Path(settings.config_path)
SERVICES_DIR = Path(settings.services_dir)

SERVICES_DIR.mkdir(parents=True, exist_ok=True)


@app.get("/config")
async def get_config():
    try:
        return read_config(CONFIG_PATH)
    except FileNotFoundError:
        raise HTTPException(status_code=404, detail="Config file not found")
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/config")
async def update_config(config: dict):
    try:
        write_config(CONFIG_PATH, config)
        return {"status": "success"}
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.get("/config/validate")
async def api_validate_config():
    is_valid, errors = validate_config(CONFIG_PATH)
    return {"valid": is_valid, "errors": errors}


@app.get("/services/available")
async def api_list_available_services():
    try:
        return list_available_services(settings.platform_api_url)
    except httpx.HTTPError as e:
        raise HTTPException(status_code=502, detail=f"Failed to fetch services: {e}")


@app.get("/services/downloaded")
async def api_list_downloaded_services():
    return list_downloaded_services(SERVICES_DIR)


@app.post("/services/download/{service_name}")
async def api_download_service(service_name: str):
    try:
        download_service(SERVICES_DIR, settings.platform_api_url, service_name)
        return {"status": "success", "service": service_name}
    except httpx.HTTPError as e:
        raise HTTPException(status_code=502, detail=f"Failed to download: {e}")
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/services/delete/{service_name}")
async def api_delete_service(service_name: str):
    try:
        delete_service(SERVICES_DIR, service_name)
        return {"status": "success", "service": service_name}
    except FileNotFoundError:
        raise HTTPException(status_code=404, detail="Service not found")
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/rebuild")
async def api_rebuild_system():
    try:
        result = rebuild_system(settings.flake_path)
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
