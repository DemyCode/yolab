import asyncio
import json
import subprocess
import tempfile
import tomllib
from pathlib import Path

import httpx
from fastapi import APIRouter, HTTPException
from fastapi.responses import StreamingResponse
from jinja2 import Template
from pydantic import BaseModel

from local_api.settings import settings

router = APIRouter()

CATALOG_DIR = Path(settings.yolab_repo_path) / "apps/catalog"
INSTALLED_APPS = Path(settings.yolab_repo_path) / "homelab/ignored/installed-apps.json"


def _tunnel_config() -> dict:
    return tomllib.loads(Path(settings.yolab_config).read_text())["tunnel"]


def _generate_wg_keypair() -> tuple[str, str]:
    private = subprocess.check_output(["wg", "genkey"], text=True).strip()
    public = subprocess.check_output(["wg", "pubkey"], input=private, text=True).strip()
    return private, public


def _load_installed() -> list[dict]:
    if not INSTALLED_APPS.exists():
        return []
    return json.loads(INSTALLED_APPS.read_text())


def _save_installed(apps: list[dict]) -> None:
    INSTALLED_APPS.parent.mkdir(parents=True, exist_ok=True)
    INSTALLED_APPS.write_text(json.dumps(apps, indent=2))


class AppInstallRequest(BaseModel):
    instance_name: str
    subdomain: str
    storage_size: str
    config: dict


@router.get("/api/tunnel/domain")
async def tunnel_domain():
    cfg = _tunnel_config()
    domain = cfg["dns_url"].removeprefix("https://").removeprefix("http://")
    return {"domain": domain}


@router.get("/api/apps/catalog")
async def catalog():
    apps = []
    for app_dir in CATALOG_DIR.iterdir():
        toml_path = app_dir / "app.toml"
        schema_path = app_dir / "schema.json"
        uischema_path = app_dir / "uischema.json"
        if not toml_path.exists():
            continue
        meta = tomllib.loads(toml_path.read_text())["app"]
        apps.append({
            "id": meta["id"],
            "name": meta["name"],
            "description": meta["description"],
            "icon": meta.get("icon", ""),
            "category": meta.get("category", ""),
            "requires_tunnel": meta.get("tunnel", False),
            "default_subdomain": meta.get("subdomain", meta["id"]),
            "schema": json.loads(schema_path.read_text()) if schema_path.exists() else {},
            "uischema": json.loads(uischema_path.read_text()) if uischema_path.exists() else {},
        })
    return apps


@router.get("/api/apps")
async def list_apps():
    return _load_installed()


@router.post("/api/apps/{app_id}")
async def install_app(app_id: str, body: AppInstallRequest):
    app_dir = CATALOG_DIR / app_id
    if not app_dir.exists():
        raise HTTPException(status_code=404, detail=f"App '{app_id}' not found in catalog")

    manifest_template = (app_dir / "manifest.yaml.j2").read_text()

    async def stream():
        tunnel_cfg = _tunnel_config()
        domain = tunnel_cfg["dns_url"].removeprefix("https://").removeprefix("http://")

        yield f"data: Generating WireGuard keypair for {app_id}...\n\n"
        wg_private_key, wg_public_key = await asyncio.to_thread(_generate_wg_keypair)

        yield "data: Registering tunnel with yolab platform...\n\n"
        async with httpx.AsyncClient(timeout=15) as client:
            resp = await client.post(
                f"{tunnel_cfg['platform_api_url']}/services",
                json={
                    "account_token": tunnel_cfg["account_token"],
                    "service_name": body.subdomain,
                    "wg_public_key": wg_public_key,
                },
            )
            if resp.status_code != 200:
                yield f"data: [ERROR] Tunnel registration failed: {resp.text}\n\n"
                return
            tunnel = resp.json()

        yield f"data: Tunnel registered — {tunnel['dns_url']}\n\n"

        yield "data: Rendering manifest...\n\n"
        rendered = Template(manifest_template).render(
            instance_name=body.instance_name,
            app_id=app_id,
            subdomain=body.subdomain,
            domain=domain,
            tunnel_service_id=tunnel["service_id"],
            storage_size=body.storage_size,
            wg_private_key=wg_private_key,
            sub_ipv6=tunnel["sub_ipv6"],
            wg_server_public_key=tunnel["wg_server_public_key"],
            wg_server_endpoint=tunnel["wg_server_endpoint"],
            **body.config,
        )

        yield "data: Applying manifests to cluster...\n\n"
        with tempfile.NamedTemporaryFile(suffix=".yaml", mode="w", delete=False) as f:
            f.write(rendered)
            manifest_path = f.name

        proc = subprocess.Popen(
            ["kubectl", "apply", "-f", manifest_path],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        for line in proc.stdout:
            yield f"data: {line.rstrip()}\n\n"
        proc.wait()
        Path(manifest_path).unlink(missing_ok=True)

        if proc.returncode != 0:
            yield f"data: [ERROR] kubectl apply failed (exit {proc.returncode})\n\n"
            return

        installed = _load_installed()
        installed.append({
            "app_id": app_id,
            "instance_name": body.instance_name,
            "subdomain": body.subdomain,
            "domain": domain,
            "tunnel_url": tunnel["dns_url"],
            "service_id": tunnel["service_id"],
            "storage_size": body.storage_size,
        })
        _save_installed(installed)

        yield f"data: [DONE] {app_id} is live at {tunnel['dns_url']}\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")
