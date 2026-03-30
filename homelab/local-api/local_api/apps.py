import json
import os
import subprocess
import tempfile
import tomllib
from pathlib import Path
from typing import Any

import httpx
import jsonschema
from fastapi import APIRouter, HTTPException
from jinja2 import Environment, FileSystemLoader, select_autoescape

REPO_PATH = os.environ.get("YOLAB_REPO_PATH", "/etc/nixos")
CATALOG_PATH = Path(os.environ.get("YOLAB_APPS_CATALOG", f"{REPO_PATH}/apps/catalog"))

KUBECONFIG = os.environ.get("KUBECONFIG", "/etc/rancher/k3s/k3s.yaml")
_KUBECTL_ENV = {**os.environ, "KUBECONFIG": KUBECONFIG}
DOMAIN = os.environ.get("YOLAB_DOMAIN", "homelab.local")
CLUSTER_CONFIG_PATH = Path(os.environ.get("YOLAB_CONFIG", "/etc/yolab/config.toml"))

router = APIRouter()


def _app_dir(app_id: str) -> Path:
    d = CATALOG_PATH / app_id
    if not d.is_dir():
        raise HTTPException(status_code=404, detail=f"App '{app_id}' not found in catalog")
    return d


def _read_meta(app_id: str) -> dict:
    with open(_app_dir(app_id) / "app.toml", "rb") as f:
        return tomllib.load(f)["app"]


def _cluster_config() -> dict:
    if not CLUSTER_CONFIG_PATH.exists():
        return {}
    with open(CLUSTER_CONFIG_PATH, "rb") as f:
        return tomllib.load(f)


def _wg_genkey() -> tuple[str, str]:
    privkey = subprocess.check_output(["wg", "genkey"]).decode().strip()
    pubkey = subprocess.check_output(["wg", "pubkey"], input=privkey.encode()).decode().strip()
    return privkey, pubkey


def _register_tunnel(service_name: str, wg_public_key: str, cluster_cfg: dict) -> dict:
    tunnel = cluster_cfg.get("tunnel", {})
    api_url = tunnel.get("platform_api_url", "https://api.yolab.dev")
    account_token = tunnel.get("account_token", "")
    if not account_token:
        raise HTTPException(status_code=400, detail="No account_token in cluster tunnel config")
    resp = httpx.post(
        f"{api_url}/services",
        json={"account_token": account_token, "service_name": service_name, "wg_public_key": wg_public_key},
        timeout=15,
    )
    if resp.status_code != 200:
        raise HTTPException(status_code=502, detail=f"Tunnel registration failed: {resp.text}")
    return resp.json()


def _delete_tunnel(service_id: int, cluster_cfg: dict) -> None:
    tunnel = cluster_cfg.get("tunnel", {})
    api_url = tunnel.get("platform_api_url", "https://api.yolab.dev")
    account_token = tunnel.get("account_token", "")
    if not account_token or not service_id:
        return
    httpx.delete(
        f"{api_url}/services/{service_id}",
        params={"user_token": account_token},
        timeout=15,
    )


def _build_pv_yaml(app_id: str, instance_name: str, volume_name: str, disk_spec: dict) -> str:
    pv_name = f"yolab-{instance_name}-{volume_name}"
    namespace = f"yolab-{instance_name}"
    pvc_name = f"{instance_name}-{volume_name}"
    disk_spec_json = json.dumps(disk_spec).replace("'", "\\'")
    return f"""apiVersion: v1
kind: PersistentVolume
metadata:
  name: {pv_name}
  labels:
    yolab.dev/app: {app_id}
    yolab.dev/instance: {instance_name}
    yolab.dev/volume: {volume_name}
spec:
  capacity:
    storage: 10Ti
  accessModes:
    - ReadWriteMany
  persistentVolumeReclaimPolicy: Retain
  storageClassName: yolab
  volumeMode: Filesystem
  csi:
    driver: csi.yolab.dev
    volumeHandle: "{instance_name}/{volume_name}"
    volumeAttributes:
      diskSpec: '{disk_spec_json}'
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {pvc_name}
  namespace: {namespace}
spec:
  storageClassName: yolab
  accessModes:
    - ReadWriteMany
  volumeName: {pv_name}
  resources:
    requests:
      storage: 1Ti
"""


def _kubectl(*args: str, **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["kubectl", "--kubeconfig", KUBECONFIG, *args],
        capture_output=True, text=True, env=_KUBECTL_ENV, **kwargs
    )


def _kubectl_apply(yaml_str: str) -> None:
    result = _kubectl("apply", "--validate=false", "-f", "-", input=yaml_str)
    if result.returncode != 0:
        raise HTTPException(status_code=500, detail=result.stderr)


# ── Catalog ───────────────────────────────────────────────────────────────────

@router.get("/api/apps")
def list_apps():
    if not CATALOG_PATH.exists():
        return []
    apps = []
    for d in sorted(CATALOG_PATH.iterdir()):
        if not d.is_dir() or not (d / "app.toml").exists():
            continue
        try:
            with open(d / "app.toml", "rb") as f:
                apps.append(tomllib.load(f)["app"])
        except Exception:
            continue
    return apps


@router.get("/api/apps/installed")
def list_installed():
    try:
        result = _kubectl("get", "namespaces", "-l", "yolab.dev/managed=true", "-o", "json", check=True)
        items = json.loads(result.stdout).get("items", [])
        return [
            {
                "app_id": item["metadata"]["labels"].get("yolab.dev/app", ""),
                "instance_name": item["metadata"]["name"].removeprefix("yolab-"),
                "namespace": item["metadata"]["name"],
                "tunnel_url": item["metadata"].get("annotations", {}).get("yolab.dev/tunnel-url", ""),
            }
            for item in items
            if item["metadata"]["labels"].get("yolab.dev/app")
        ]
    except Exception:
        return []


@router.get("/api/apps/{app_id}")
def get_app(app_id: str):
    return _read_meta(app_id)


@router.get("/api/apps/{app_id}/schema")
def get_schema(app_id: str):
    meta = _read_meta(app_id)
    f = _app_dir(app_id) / "schema.json"
    schema = json.loads(f.read_text()) if f.exists() else {
        "title": meta["name"], "type": "object", "required": [], "properties": {}
    }
    schema.setdefault("required", [])
    schema.setdefault("properties", {})
    title = "Subdomain" if meta.get("tunnel") else "Instance name"
    description = "Becomes part of your public URL" if meta.get("tunnel") else "Unique name for this installation"
    instance_field = {
        "type": "string",
        "title": title,
        "description": description,
        "default": app_id,
        "minLength": 2,
        "pattern": "^[a-z0-9-]+$",
    }
    schema["properties"] = {"instance_name": instance_field, **schema["properties"]}
    if "instance_name" not in schema["required"]:
        schema["required"] = ["instance_name"] + schema["required"]
    return schema


@router.get("/api/apps/{app_id}/uischema")
def get_uischema(app_id: str):
    f = _app_dir(app_id) / "uischema.json"
    if not f.exists():
        return {}
    return json.loads(f.read_text())


@router.get("/api/apps/{app_id}/status")
def get_status(app_id: str, instance_name: str | None = None):
    namespace = f"yolab-{instance_name or app_id}"
    try:
        result = _kubectl("get", "pods", "-n", namespace, "-o", "json", check=True)
        pods = [
            {
                "name": p["metadata"]["name"],
                "phase": p["status"].get("phase", "Unknown"),
            }
            for p in json.loads(result.stdout).get("items", [])
        ]
        if not pods:
            overall = "starting"
        elif all(p["phase"] == "Running" for p in pods):
            overall = "running"
        elif any(p["phase"] == "Failed" for p in pods):
            overall = "error"
        else:
            overall = "starting"
        return {"status": overall, "pods": pods}
    except subprocess.CalledProcessError:
        return {"status": "not_installed", "pods": []}


# ── Install / Uninstall ───────────────────────────────────────────────────────

@router.post("/api/apps/{app_id}/install")
def install_app(app_id: str, config: dict[str, Any]):
    app_dir = _app_dir(app_id)
    meta = _read_meta(app_id)

    instance_name: str = config.pop("instance_name", app_id)
    volumes_selection: dict[str, dict] = config.pop("volumes", {})

    schema_file = app_dir / "schema.json"
    if schema_file.exists():
        try:
            jsonschema.validate(
                instance=config,
                schema=json.loads(schema_file.read_text()),
            )
        except jsonschema.ValidationError as e:
            raise HTTPException(status_code=422, detail=e.message)

    template_file = app_dir / "manifest.yaml.j2"
    if not template_file.exists():
        raise HTTPException(status_code=500, detail="No manifest template found for this app")

    render_ctx: dict[str, Any] = {**config, "domain": DOMAIN, "app_id": app_id, "instance_name": instance_name}

    if meta.get("tunnel"):
        cluster_cfg = _cluster_config()
        wg_private_key, wg_public_key = _wg_genkey()
        tunnel_info = _register_tunnel(instance_name, wg_public_key, cluster_cfg)
        render_ctx.update(
            wg_private_key=wg_private_key,
            sub_ipv6=tunnel_info["sub_ipv6"],
            wg_server_endpoint=tunnel_info["wg_server_endpoint"],
            wg_server_public_key=tunnel_info["wg_server_public_key"],
            tunnel_service_id=tunnel_info["service_id"],
            subdomain=instance_name,
        )

    namespace = f"yolab-{instance_name}"

    if volumes_selection:
        ns_yaml = f"""apiVersion: v1
kind: Namespace
metadata:
  name: {namespace}
  labels:
    yolab.dev/managed: "true"
    yolab.dev/app: "{app_id}"
"""
        _kubectl_apply(ns_yaml)

        for vol_name, disk_spec in volumes_selection.items():
            _kubectl_apply(_build_pv_yaml(app_id, instance_name, vol_name, disk_spec))

    env = Environment(
        loader=FileSystemLoader(str(app_dir)),
        autoescape=select_autoescape([]),
    )
    manifest = env.get_template("manifest.yaml.j2").render(**render_ctx)

    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as tmp:
        tmp.write(manifest)
        tmp_path = tmp.name

    try:
        result = _kubectl("apply", "--validate=false", "-f", tmp_path)
        if result.returncode != 0:
            raise HTTPException(status_code=500, detail=result.stderr)
        return {"ok": True, "output": result.stdout}
    finally:
        Path(tmp_path).unlink(missing_ok=True)


@router.delete("/api/apps/{app_id}")
def uninstall_app(app_id: str, instance_name: str | None = None, wipe: bool = False):
    effective = instance_name or app_id
    namespace = f"yolab-{effective}"

    try:
        ns_result = _kubectl("get", "namespace", namespace, "-o", "json")
        if ns_result.returncode == 0:
            ns_data = json.loads(ns_result.stdout)
            annotations = ns_data["metadata"].get("annotations", {})
            service_id_str = annotations.get("yolab.dev/service-id", "")
            if service_id_str:
                cluster_cfg = _cluster_config()
                _delete_tunnel(int(service_id_str), cluster_cfg)
    except Exception:
        pass

    if wipe:
        _kubectl("delete", "namespace", namespace, "--ignore-not-found")
        _kubectl("delete", "pv", "-l", f"yolab.dev/instance={effective}", "--ignore-not-found")
    else:
        result = _kubectl("delete", "deployments,services,ingress,secrets",
                          "--all", "-n", namespace, "--ignore-not-found")
        if result.returncode != 0:
            raise HTTPException(status_code=500, detail=result.stderr)

    return {"ok": True}
