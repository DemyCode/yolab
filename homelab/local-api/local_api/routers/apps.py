import asyncio
import json
import re
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

LABEL_MANAGED = "yolab.io/managed"
ANN_APP_ID    = "yolab.io/app-id"
ANN_TUNNEL_URL = "yolab.io/tunnel-url"
ANN_OUTPUTS   = "yolab.io/outputs"
ANN_SERVICE_IDS = "yolab.io/service-ids"
ANN_CONFIG    = "yolab.io/config"


def _tunnel_config() -> dict:
    return tomllib.loads(Path(settings.yolab_config).read_text())["tunnel"]


def _generate_wg_keypair() -> tuple[str, str]:
    private = subprocess.check_output(["wg", "genkey"], text=True).strip()
    public = subprocess.check_output(["wg", "pubkey"], input=private, text=True).strip()
    return private, public


def _normalize_outputs(ann: dict) -> list[dict]:
    raw = ann.get(ANN_OUTPUTS, "")
    if raw:
        outputs = json.loads(raw)
        # Convert old format [{url, ipv6}] to new AppOutput format
        if outputs and isinstance(outputs[0], dict) and ("url" in outputs[0] or "ipv6" in outputs[0]):
            result = []
            for o in outputs:
                if o.get("url"):
                    result.append({"key": "url", "label": "Web URL", "value": o["url"], "type": "url"})
                if o.get("ipv6"):
                    result.append({"key": "ipv6", "label": "IPv6", "value": o["ipv6"], "type": "text"})
            return result
        return outputs
    tunnel_url = ann.get(ANN_TUNNEL_URL, "")
    if tunnel_url:
        return [{"key": "url", "label": "Web URL", "value": tunnel_url, "type": "url"}]
    return []


def _list_installed() -> list[dict]:
    result = subprocess.run(
        ["kubectl", "get", "namespaces", "-l", f"{LABEL_MANAGED}=true", "-o", "json"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        return []
    items = json.loads(result.stdout).get("items", [])
    apps = []
    for ns in items:
        ann = ns.get("metadata", {}).get("annotations", {})
        name = ns["metadata"]["name"].removeprefix("yolab-")
        phase = ns.get("status", {}).get("phase", "Active")
        if phase == "Terminating":
            status = "uninstalling"
        else:
            pods = subprocess.run(
                ["kubectl", "get", "pods", "-n", f"yolab-{name}", "-o", "json"],
                capture_output=True, text=True,
            )
            if pods.returncode == 0:
                pod_items = json.loads(pods.stdout).get("items", [])
                all_ready = pod_items and all(
                    any(c["type"] == "Ready" and c["status"] == "True"
                        for c in p.get("status", {}).get("conditions", []))
                    for p in pod_items
                )
                status = "running" if all_ready else "starting"
            else:
                status = "starting"

        config_raw = ann.get(ANN_CONFIG, "")
        config = json.loads(config_raw) if config_raw else {}

        apps.append({
            "app_id": ann.get(ANN_APP_ID, ""),
            "instance_name": name,
            "status": status,
            "outputs": _normalize_outputs(ann),
            "config": config,
        })
    return apps


def _annotate_namespace(instance_name: str, app_id: str, tunnel_url: str, outputs: list[dict], service_ids: list[int]) -> None:
    ns = f"yolab-{instance_name}"
    subprocess.run(
        ["kubectl", "label", "namespace", ns, f"{LABEL_MANAGED}=true", "--overwrite=true"],
        capture_output=True,
    )
    subprocess.run(
        ["kubectl", "annotate", "namespace", ns,
         f"{ANN_APP_ID}={app_id}", f"{ANN_TUNNEL_URL}={tunnel_url}",
         f"{ANN_OUTPUTS}={json.dumps(outputs)}",
         f"{ANN_SERVICE_IDS}={json.dumps(service_ids)}", "--overwrite=true"],
        capture_output=True,
    )


def _delete_tunnels(tunnel_ids: list[int]) -> None:
    if not tunnel_ids:
        return
    tunnel_cfg = tomllib.loads(Path(settings.yolab_config).read_text())["tunnel"]
    for tid in tunnel_ids:
        try:
            import urllib.request
            req = urllib.request.Request(
                f"{tunnel_cfg['platform_api_url']}/tunnels/{tid}?account_token={tunnel_cfg['account_token']}",
                method="DELETE",
            )
            urllib.request.urlopen(req, timeout=10)
        except Exception as e:
            print(f"[warn] failed to delete tunnel {tid}: {e}")


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
        auto_keys = {k for k, v in schema.get("properties", {}).items() if v.get("x-auto")}
        user_schema = {
            **schema,
            "properties": {k: v for k, v in schema.get("properties", {}).items() if k not in auto_keys},
            "required": [r for r in schema.get("required", []) if r not in auto_keys],
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
    return await asyncio.to_thread(_list_installed)


@router.post("/api/apps/{app_id}")
async def install_app(app_id: str, body: AppInstallRequest):
    if not re.match(r"^[a-z0-9-]+$", body.instance_name):
        raise HTTPException(status_code=400, detail="instance_name must be lowercase alphanumeric and hyphens only")

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
      try:
        tunnel_cfg = _tunnel_config()
        tunnel_vars = {}

        registered_service_ids = []

        for field in tunnel_fields:
            subdomain = body.config.get(field, field)
            yield f"data: Generating WireGuard keypair for '{subdomain}'...\n\n"
            wg_private_key, wg_public_key = await asyncio.to_thread(_generate_wg_keypair)

            yield f"data: Registering tunnel '{subdomain}'...\n\n"
            async with httpx.AsyncClient(timeout=15) as client:
                # Step 1: create the WireGuard tunnel
                resp = await client.post(
                    f"{tunnel_cfg['platform_api_url']}/tunnels",
                    json={
                        "account_token": tunnel_cfg["account_token"],
                        "wg_public_key": wg_public_key,
                    },
                )
                if resp.status_code != 200:
                    yield f"data: [ERROR] Tunnel creation failed: {resp.text}\n\n"
                    await asyncio.to_thread(_delete_tunnels, registered_service_ids)
                    return
                tunnel_data = resp.json()
                tunnel_id = tunnel_data["tunnel_id"]
                sub_ipv6 = tunnel_data["sub_ipv6"]

                # Step 2: attach AAAA record for the subdomain
                record_resp = await client.post(
                    f"{tunnel_cfg['platform_api_url']}/tunnels/{tunnel_id}/records",
                    json={
                        "account_token": tunnel_cfg["account_token"],
                        "record_type": "AAAA",
                        "name": subdomain,
                        "value": sub_ipv6,
                    },
                )
                if record_resp.status_code != 200:
                    yield f"data: [ERROR] DNS record creation failed: {record_resp.text}\n\n"
                    await asyncio.to_thread(_delete_tunnels, registered_service_ids + [tunnel_id])
                    return
                fqdn = record_resp.json()["fqdn"]

            url = f"https://{fqdn}"
            registered_service_ids.append(tunnel_id)
            tunnel_vars[f"{field}_tunnel"] = {
                "url": url,
                "domain": fqdn,
                "tunnel_id": tunnel_id,
                "sub_ipv6": sub_ipv6,
                "wg_private_key": wg_private_key,
                "wg_server_public_key": tunnel_data["wg_server_public_key"],
                "wg_server_endpoint": tunnel_data["wg_server_endpoint"],
            }

            yield f"data: Tunnel registered — {url}\n\n"

        disk = body.config.get("disk")
        template_disk = disk
        if isinstance(disk, dict):
            yolab_path = str(Path(disk["path"]) / "yolab")
            template_disk = {**disk, "path": yolab_path}
            if disk.get("host") == settings.yolab_node_ipv6:
                try:
                    (Path(yolab_path) / body.instance_name).mkdir(parents=True, exist_ok=True)
                except OSError as e:
                    import errno as _errno
                    if e.errno == _errno.EROFS:
                        yield f"data: [ERROR] Disk at {disk['path']} is mounted read-only (Windows Fast Startup?). Go to the Disks page, click Unexport then Export as NFS to remount it.\n\n"
                        return
                    raise

        yield "data: Rendering manifest...\n\n"
        config_with_disk = {**body.config, "disk": template_disk} if disk else body.config
        rendered = Template(manifest_template).render(
            instance_name=body.instance_name,
            app_id=app_id,
            **config_with_disk,
            **auto_fields,
            **tunnel_vars,
        )

        yield "data: Applying manifests to cluster...\n\n"
        with tempfile.NamedTemporaryFile(suffix=".yaml", mode="w", delete=False) as f:
            f.write(rendered)
            manifest_path = f.name

        proc = await asyncio.create_subprocess_exec(
            "kubectl", "apply", "-f", manifest_path,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        while True:
            line = await proc.stdout.readline()
            if not line:
                break
            yield f"data: {line.decode().rstrip()}\n\n"
        await proc.wait()
        Path(manifest_path).unlink(missing_ok=True)

        if proc.returncode != 0:
            yield f"data: [ERROR] kubectl apply failed (exit {proc.returncode})\n\n"
            return

        # Store user config (excluding auto-generated secrets) in namespace annotation
        user_config = {k: v for k, v in body.config.items() if k not in auto_fields}
        await asyncio.to_thread(
            subprocess.run,
            ["kubectl", "annotate", "namespace", f"yolab-{body.instance_name}",
             f"{ANN_CONFIG}={json.dumps(user_config)}", "--overwrite=true"],
            capture_output=True,
        )

        outputs = [
            {"key": "url", "label": "Web URL", "value": t["url"], "type": "url"}
            for t in tunnel_vars.values()
        ]
        primary_url = tunnel_vars[list(tunnel_vars.keys())[0]]["url"] if tunnel_vars else ""
        await asyncio.to_thread(_annotate_namespace, body.instance_name, app_id, primary_url, outputs, registered_service_ids)

        yield f"data: [DONE] {app_id} installed\n\n"

      except Exception as e:
        yield f"data: [ERROR] {type(e).__name__}: {e}\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")


@router.post("/api/apps/{instance_name}/scan-outputs")
async def scan_outputs(instance_name: str):
    ns = f"yolab-{instance_name}"

    ns_info = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "get", "namespace", ns, "-o", "json"],
        capture_output=True, text=True,
    )
    if ns_info.returncode != 0:
        raise HTTPException(status_code=404, detail="Instance not found")

    ns_data = json.loads(ns_info.stdout)
    ann = ns_data.get("metadata", {}).get("annotations", {})
    app_id = ann.get(ANN_APP_ID, "")

    # Try outputs.json spec for this app
    outputs_spec = []
    outputs_json_path = CATALOG_DIR / app_id / "outputs.json"
    if outputs_json_path.exists():
        outputs_spec = json.loads(outputs_json_path.read_text())

    if not outputs_spec:
        # No spec: return existing annotation outputs
        return {"outputs": _normalize_outputs(ann)}

    # Scan all pod logs for YOLAB_OUTPUT lines
    pods_result = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "get", "pods", "-n", ns, "-o", "json"],
        capture_output=True, text=True,
    )

    found: dict[str, str] = {}
    if pods_result.returncode == 0:
        pod_items = json.loads(pods_result.stdout).get("items", [])
        for pod in pod_items:
            pod_name = pod["metadata"]["name"]
            init_containers = [c["name"] for c in pod.get("spec", {}).get("initContainers", [])]
            containers = [c["name"] for c in pod.get("spec", {}).get("containers", [])]
            for container in init_containers + containers:
                logs_result = await asyncio.to_thread(
                    subprocess.run,
                    ["kubectl", "logs", "-n", ns, pod_name, "-c", container, "--tail=500"],
                    capture_output=True, text=True,
                )
                if logs_result.returncode == 0:
                    for line in logs_result.stdout.splitlines():
                        m = re.match(r"^YOLAB_OUTPUT (\S+) (.+)$", line.strip())
                        if m:
                            found[m.group(1)] = m.group(2).strip()

    if not found:
        return {"outputs": _normalize_outputs(ann)}

    outputs = []
    service_ids = []
    for spec in outputs_spec:
        key = spec["key"]
        if key in found:
            outputs.append({"key": key, "label": spec.get("label", key), "value": found[key], "type": spec.get("type", "text")})
            if key == "tunnel_id":
                try:
                    service_ids.append(int(found[key]))
                except ValueError:
                    pass

    annotations_to_set = [f"{ANN_OUTPUTS}={json.dumps(outputs)}"]
    if service_ids:
        annotations_to_set.append(f"{ANN_SERVICE_IDS}={json.dumps(service_ids)}")
    await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "annotate", "namespace", ns, *annotations_to_set, "--overwrite=true"],
        capture_output=True,
    )

    return {"outputs": outputs}


@router.post("/api/apps/{instance_name}/force-uninstall")
async def force_uninstall_app(instance_name: str):
    ns = f"yolab-{instance_name}"
    await asyncio.to_thread(subprocess.run, ["kubectl", "patch", "namespace", ns, "-p", '{"metadata":{"finalizers":[]}}', "--type=merge"], capture_output=True, text=True)
    await asyncio.to_thread(subprocess.run, ["kubectl", "delete", "namespace", ns, "--force", "--grace-period=0", "--ignore-not-found=true"], capture_output=True, text=True)
    await asyncio.to_thread(subprocess.run, ["kubectl", "patch", "pv", f"{ns}-data", "-p", '{"metadata":{"finalizers":[]}}', "--type=merge"], capture_output=True, text=True)
    await asyncio.to_thread(subprocess.run, ["kubectl", "delete", "pv", f"{ns}-data", "--force", "--grace-period=0", "--ignore-not-found=true"], capture_output=True, text=True)
    return {"ok": True}


@router.delete("/api/apps/{instance_name}")
async def uninstall_app(instance_name: str):
    ns = f"yolab-{instance_name}"
    ns_info = await asyncio.to_thread(subprocess.run, ["kubectl", "get", "namespace", ns, "-o", "json", "--ignore-not-found=true"], capture_output=True, text=True)
    service_ids = []
    if ns_info.returncode == 0 and ns_info.stdout.strip():
        ann = json.loads(ns_info.stdout).get("metadata", {}).get("annotations", {})
        raw = ann.get(ANN_SERVICE_IDS, "")
        if raw:
            service_ids = json.loads(raw)

    result = await asyncio.to_thread(subprocess.run, ["kubectl", "delete", "namespace", ns, "--ignore-not-found=true", "--wait=false"], capture_output=True, text=True)
    if result.returncode != 0:
        raise HTTPException(status_code=500, detail=result.stderr.strip())

    await asyncio.to_thread(subprocess.run, ["kubectl", "delete", "pv", f"{ns}-data", "--ignore-not-found=true", "--wait=false"], capture_output=True, text=True)
    await asyncio.to_thread(_delete_tunnels, service_ids)
    return {"ok": True}


@router.get("/api/apps/{instance_name}/pods")
async def list_pods(instance_name: str):
    result = await asyncio.to_thread(subprocess.run, ["kubectl", "get", "pods", "-n", f"yolab-{instance_name}", "-o", "json"], capture_output=True, text=True)
    if result.returncode != 0:
        raise HTTPException(status_code=404, detail=result.stderr)
    items = json.loads(result.stdout).get("items", [])
    return [
        {"name": p["metadata"]["name"], "phase": p["status"].get("phase", "Unknown"),
         "ready": any(c["type"] == "Ready" and c["status"] == "True" for c in p["status"].get("conditions", []))}
        for p in items
    ]


@router.get("/api/apps/{instance_name}/describe/{pod_name}")
async def describe_pod(instance_name: str, pod_name: str):
    result = await asyncio.to_thread(subprocess.run, ["kubectl", "describe", "pod", pod_name, "-n", f"yolab-{instance_name}"], capture_output=True, text=True)
    return {"output": result.stdout + result.stderr}


@router.get("/api/apps/{instance_name}/logs/{pod_name}")
async def pod_logs(instance_name: str, pod_name: str):
    async def stream():
        proc = await asyncio.create_subprocess_exec(
            "kubectl", "logs", "-n", f"yolab-{instance_name}", pod_name,
            "--all-containers=true", "--follow", "--prefix=true", "--tail=100",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
        )
        while True:
            line = await proc.stdout.readline()
            if not line:
                break
            yield f"data: {line.decode().rstrip()}\n\n"
    return StreamingResponse(stream(), media_type="text/event-stream")
