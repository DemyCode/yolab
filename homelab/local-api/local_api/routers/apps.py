import asyncio
import json
import secrets
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
    config: dict


@router.get("/api/tunnel/domain")
async def tunnel_domain():
    cfg = _tunnel_config()
    host = cfg["dns_url"].removeprefix("https://").removeprefix("http://")
    suffix = host.split(".", 1)[1] if "." in host else host
    return {"domain": suffix}


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
        schema = json.loads(schema_path.read_text()) if schema_path.exists() else {}
        user_schema = {
            **schema,
            "properties": {
                k: v for k, v in schema.get("properties", {}).items()
                if not v.get("x-auto")
            },
        }
        apps.append({
            "id": meta["id"],
            "name": meta["name"],
            "description": meta["description"],
            "icon": meta.get("icon", ""),
            "category": meta.get("category", ""),
            "schema": user_schema,
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

    schema_path = app_dir / "schema.json"
    schema = json.loads(schema_path.read_text()) if schema_path.exists() else {}
    properties = schema.get("properties", {})

    tunnel_fields = [k for k, v in properties.items() if v.get("format") == "tunnel"]
    auto_fields = {k: secrets.token_urlsafe(32) for k, v in properties.items() if v.get("x-auto") == "password"}

    manifest_template = (app_dir / "manifest.yaml.j2").read_text()

    async def stream():
        tunnel_cfg = _tunnel_config()
        tunnel_vars = {}
        tunnel_urls = []

        for field in tunnel_fields:
            subdomain = body.config.get(field, field)
            yield f"data: Generating WireGuard keypair for '{subdomain}'...\n\n"
            wg_private_key, wg_public_key = await asyncio.to_thread(_generate_wg_keypair)

            yield f"data: Registering tunnel '{subdomain}'...\n\n"
            async with httpx.AsyncClient(timeout=15) as client:
                resp = await client.post(
                    f"{tunnel_cfg['platform_api_url']}/services",
                    json={
                        "account_token": tunnel_cfg["account_token"],
                        "service_name": subdomain,
                        "wg_public_key": wg_public_key,
                    },
                )
                if resp.status_code != 200:
                    yield f"data: [ERROR] Tunnel registration failed: {resp.text}\n\n"
                    return
                tunnel = resp.json()

            url = tunnel["dns_url"]
            domain = url.removeprefix("https://").removeprefix("http://")
            tunnel_vars[f"{field}_tunnel"] = {
                "url": url,
                "domain": domain,
                "service_id": tunnel["service_id"],
                "sub_ipv6": tunnel["sub_ipv6"],
                "wg_private_key": wg_private_key,
                "wg_server_public_key": tunnel["wg_server_public_key"],
                "wg_server_endpoint": tunnel["wg_server_endpoint"],
            }
            tunnel_urls.append(url)
            yield f"data: Tunnel registered — {url}\n\n"

        yield "data: Rendering manifest...\n\n"
        rendered = Template(manifest_template).render(
            instance_name=body.instance_name,
            app_id=app_id,
            **body.config,
            **auto_fields,
            **tunnel_vars,
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
            "tunnel_urls": tunnel_urls,
            "tunnel_url": tunnel_urls[0] if tunnel_urls else "",
        })
        _save_installed(installed)

        yield f"data: [DONE] {app_id} is live at {tunnel_urls[0] if tunnel_urls else 'cluster'}\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")
