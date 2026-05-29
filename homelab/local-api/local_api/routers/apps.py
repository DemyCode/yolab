import asyncio
import json
import re
import subprocess
import tempfile
import tomllib
from pathlib import Path
from typing import AsyncGenerator

from fastapi import APIRouter, HTTPException
from fastapi.responses import StreamingResponse
from jinja2 import Template
from pydantic import BaseModel

from local_api.settings import settings

router = APIRouter()

CATALOG_DIR = Path(settings.yolab_repo_path) / "apps/catalog"

LABEL_MANAGED = "yolab.io/managed"
ANN_APP_ID = "yolab.io/app-id"
ANN_OUTPUTS = "yolab.io/outputs"
ANN_CONFIG = "yolab.io/config"


def _tunnel_config() -> dict:
    return tomllib.loads(Path(settings.yolab_config).read_text())["tunnel"]


def _normalize_outputs(ann: dict) -> list[dict]:
    raw = ann.get(ANN_OUTPUTS, "")
    if not raw:
        return []
    outputs = json.loads(raw)
    # Convert old format [{url, ipv6}] to new AppOutput format
    if (
        outputs
        and isinstance(outputs[0], dict)
        and ("url" in outputs[0] or "ipv6" in outputs[0])
    ):
        result = []
        for o in outputs:
            if o.get("url"):
                result.append(
                    {"key": "url", "label": "Web URL", "value": o["url"], "type": "url"}
                )
            if o.get("ipv6"):
                result.append(
                    {"key": "ipv6", "label": "IPv6", "value": o["ipv6"], "type": "text"}
                )
        return result
    return outputs


def _list_installed() -> list[dict]:
    result = subprocess.run(
        ["kubectl", "get", "namespaces", "-l", f"{LABEL_MANAGED}=true", "-o", "json"],
        capture_output=True,
        text=True,
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
                capture_output=True,
                text=True,
            )
            if pods.returncode == 0:
                pod_items = json.loads(pods.stdout).get("items", [])
                all_ready = pod_items and all(
                    any(
                        c["type"] == "Ready" and c["status"] == "True"
                        for c in p.get("status", {}).get("conditions", [])
                    )
                    for p in pod_items
                )
                status = "running" if all_ready else "starting"
            else:
                status = "starting"

        config_raw = ann.get(ANN_CONFIG, "")
        config = json.loads(config_raw) if config_raw else {}

        app_id = ann.get(ANN_APP_ID, "")
        outputs_spec_path = CATALOG_DIR / app_id / "outputs.json" if app_id else None
        outputs_spec = []
        if outputs_spec_path and outputs_spec_path.exists():
            try:
                outputs_spec = [
                    {
                        "key": o["key"],
                        "label": o.get("label", o["key"]),
                        "type": o.get("type", "text"),
                    }
                    for o in json.loads(outputs_spec_path.read_text())
                    if o.get("type") != "hidden"
                ]
            except Exception:
                pass

        apps.append(
            {
                "app_id": app_id,
                "instance_name": name,
                "status": status,
                "outputs": _normalize_outputs(ann),
                "outputs_spec": outputs_spec,
                "config": config,
            }
        )
    return apps


# ─── Shared helpers ───────────────────────────────────────────────────────────


def _render_manifest(
    app_id: str,
    instance_name: str,
    config: dict,
    tunnel_cfg: dict,
    template_file: str = "manifest.yaml.j2",
    extra_vars: dict | None = None,
) -> str:
    app_dir = CATALOG_DIR / app_id
    schema_path = app_dir / "schema.json"
    schema = json.loads(schema_path.read_text()) if schema_path.exists() else {}
    properties = schema.get("properties", {})
    tunnel_field = next(
        (k for k, v in properties.items() if v.get("format") == "tunnel"), None
    )
    service_name = config.get(tunnel_field, "") if tunnel_field else ""

    return Template((app_dir / template_file).read_text()).render(
        instance_name=instance_name,
        app_id=app_id,
        platform_api_url=tunnel_cfg["platform_api_url"],
        account_token=tunnel_cfg["account_token"],
        service_name=service_name,
        **(extra_vars or {}),
        **config,
    )


async def _stream_proc(
    *cmd: str,
) -> AsyncGenerator[tuple[str | None, int | None], None]:
    """Run a command, yielding (line, None) for each output line then (None, returncode)."""
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )
    assert proc.stdout is not None
    while True:
        line = await proc.stdout.readline()
        if not line:
            break
        yield line.decode().rstrip(), None
    await proc.wait()
    yield None, proc.returncode


async def _apply_manifest(
    rendered: str,
) -> AsyncGenerator[tuple[str | None, int | None], None]:
    """Write rendered YAML to a temp file, kubectl apply it, and stream output."""
    with tempfile.NamedTemporaryFile(suffix=".yaml", mode="w", delete=False) as f:
        f.write(rendered)
        path = f.name
    try:
        async for item in _stream_proc("kubectl", "apply", "-f", path):
            yield item
    finally:
        Path(path).unlink(missing_ok=True)


# ─── Routes ───────────────────────────────────────────────────────────────────


class AppInstallRequest(BaseModel):
    instance_name: str
    config: dict


@router.get("/tunnel/domain")
async def tunnel_domain():
    cfg = _tunnel_config()
    host = cfg["dns_url"].removeprefix("https://").removeprefix("http://").rstrip("/")
    parts = host.split(".")
    if parts and not parts[0].isdigit():
        host = ".".join(parts[1:])
    return {"domain": host}


@router.get("/apps/catalog")
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
        apps.append(
            {
                "id": meta["id"],
                "name": meta["name"],
                "description": meta["description"],
                "icon": meta.get("icon", ""),
                "category": meta.get("category", ""),
                "schema": schema,
                "uischema": json.loads(uischema_path.read_text())
                if uischema_path.exists()
                else {},
            }
        )
    return apps


@router.get("/apps")
async def list_apps():
    return await asyncio.to_thread(_list_installed)


@router.post("/apps/{app_id}")
async def install_app(app_id: str, body: AppInstallRequest):
    if not re.match(r"^[a-z0-9-]+$", body.instance_name):
        raise HTTPException(
            status_code=400,
            detail="instance_name must be lowercase alphanumeric and hyphens only",
        )

    if not (CATALOG_DIR / app_id).exists():
        raise HTTPException(
            status_code=404, detail=f"App '{app_id}' not found in catalog"
        )

    async def stream():
        try:
            tunnel_cfg = _tunnel_config()

            yield "data: Rendering manifest...\n\n"
            rendered = _render_manifest(
                app_id, body.instance_name, body.config, tunnel_cfg
            )

            yield "data: Applying manifests to cluster...\n\n"
            rc = None
            async for line, code in _apply_manifest(rendered):
                if code is not None:
                    rc = code
                elif line:
                    yield f"data: {line}\n\n"
            if rc != 0:
                yield f"data: [ERROR] kubectl apply failed (exit {rc})\n\n"
                return

            await asyncio.to_thread(
                subprocess.run,
                [
                    "kubectl",
                    "annotate",
                    "namespace",
                    f"yolab-{body.instance_name}",
                    f"{ANN_CONFIG}={json.dumps(body.config)}",
                    "--overwrite=true",
                ],
                capture_output=True,
            )

            yield f"data: [DONE] {app_id} installed — run 'Scan outputs' once the pod is ready\n\n"

        except Exception as e:
            yield f"data: [ERROR] {type(e).__name__}: {e}\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")


@router.post("/apps/{instance_name}/update")
async def update_app(instance_name: str):
    ns = f"yolab-{instance_name}"

    ns_info = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "get", "namespace", ns, "-o", "json"],
        capture_output=True,
        text=True,
    )
    if ns_info.returncode != 0:
        raise HTTPException(status_code=404, detail="Instance not found")

    ann = json.loads(ns_info.stdout).get("metadata", {}).get("annotations", {})
    app_id = ann.get(ANN_APP_ID, "")
    config = json.loads(ann.get(ANN_CONFIG, "{}") or "{}")

    if not app_id:
        raise HTTPException(status_code=400, detail="No app ID found on namespace")
    if not (CATALOG_DIR / app_id).exists():
        raise HTTPException(
            status_code=404, detail=f"App '{app_id}' not found in catalog"
        )

    async def stream():
        try:
            tunnel_cfg = _tunnel_config()

            yield "data: Rendering manifest...\n\n"
            rendered = _render_manifest(app_id, instance_name, config, tunnel_cfg)

            yield "data: Applying updated manifests...\n\n"
            rc = None
            async for line, code in _apply_manifest(rendered):
                if code is not None:
                    rc = code
                elif line:
                    yield f"data: {line}\n\n"
            if rc != 0:
                yield f"data: [ERROR] kubectl apply failed (exit {rc})\n\n"
                return

            yield "data: Restarting deployments...\n\n"
            async for line, _ in _stream_proc(
                "kubectl", "rollout", "restart", "deployment", "-n", ns
            ):
                if line:
                    yield f"data: {line}\n\n"

            yield f"data: [DONE] {app_id} updated\n\n"

        except Exception as e:
            yield f"data: [ERROR] {type(e).__name__}: {e}\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")


@router.post("/apps/{instance_name}/scan-outputs")
async def scan_outputs(instance_name: str):
    ns = f"yolab-{instance_name}"

    ns_info = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "get", "namespace", ns, "-o", "json"],
        capture_output=True,
        text=True,
    )
    if ns_info.returncode != 0:
        raise HTTPException(status_code=404, detail="Instance not found")

    ns_data = json.loads(ns_info.stdout)
    ann = ns_data.get("metadata", {}).get("annotations", {})
    app_id = ann.get(ANN_APP_ID, "")

    outputs_json_path = CATALOG_DIR / app_id / "outputs.json"
    if not outputs_json_path.exists():
        return {"outputs": _normalize_outputs(ann)}

    outputs_spec = json.loads(outputs_json_path.read_text())

    # Scan all init container + container logs for pattern matches
    pods_result = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "get", "pods", "-n", ns, "-o", "json"],
        capture_output=True,
        text=True,
    )

    found: dict[str, str] = {}
    if pods_result.returncode == 0:
        pod_items = json.loads(pods_result.stdout).get("items", [])
        for pod in pod_items:
            pod_name = pod["metadata"]["name"]
            init_containers = [
                c["name"] for c in pod.get("spec", {}).get("initContainers", [])
            ]
            containers = [c["name"] for c in pod.get("spec", {}).get("containers", [])]
            for container in init_containers + containers:
                logs_result = await asyncio.to_thread(
                    subprocess.run,
                    [
                        "kubectl",
                        "logs",
                        "-n",
                        ns,
                        pod_name,
                        "-c",
                        container,
                        "--tail=500",
                    ],
                    capture_output=True,
                    text=True,
                )
                if logs_result.returncode != 0:
                    continue
                for line in logs_result.stdout.splitlines():
                    for spec in outputs_spec:
                        key = spec["key"]
                        if key not in found:
                            m = re.search(spec["pattern"], line)
                            if m:
                                found[key] = str(m.group(1))

    if not found:
        return {"outputs": _normalize_outputs(ann)}

    outputs = []
    for spec in outputs_spec:
        key = spec["key"]
        if key in found:
            outputs.append(
                {
                    "key": key,
                    "label": spec.get("label", key),
                    "value": found[key],
                    "type": spec.get("type", "text"),
                }
            )

    await asyncio.to_thread(
        subprocess.run,
        [
            "kubectl",
            "annotate",
            "namespace",
            ns,
            f"{ANN_OUTPUTS}={json.dumps(outputs)}",
            "--overwrite=true",
        ],
        capture_output=True,
    )

    return {"outputs": outputs}


@router.delete("/apps/{instance_name}")
async def uninstall_app(instance_name: str):
    ns = f"yolab-{instance_name}"
    ns_info = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "get", "namespace", ns, "-o", "json", "--ignore-not-found=true"],
        capture_output=True,
        text=True,
    )

    if ns_info.returncode == 0 and ns_info.stdout.strip():
        ann = json.loads(ns_info.stdout).get("metadata", {}).get("annotations", {})
        app_id = ann.get(ANN_APP_ID, "")
        uninstall_j2 = CATALOG_DIR / app_id / "uninstall.yaml.j2" if app_id else None

        if uninstall_j2 and uninstall_j2.exists():
            try:
                config = json.loads(ann.get(ANN_CONFIG, "{}") or "{}")
                outputs = _normalize_outputs(ann)
                output_vars = {f"output_{o['key']}": o["value"] for o in outputs}
                tunnel_cfg = _tunnel_config()
                rendered = _render_manifest(
                    app_id,
                    instance_name,
                    config,
                    tunnel_cfg,
                    template_file="uninstall.yaml.j2",
                    extra_vars=output_vars,
                )
                with tempfile.NamedTemporaryFile(
                    suffix=".yaml", mode="w", delete=False
                ) as f:
                    f.write(rendered)
                    manifest_path = f.name
                await asyncio.to_thread(
                    subprocess.run,
                    ["kubectl", "apply", "-f", manifest_path],
                    capture_output=True,
                )
                Path(manifest_path).unlink(missing_ok=True)
                await asyncio.to_thread(
                    subprocess.run,
                    [
                        "kubectl",
                        "wait",
                        "job/uninstall",
                        "-n",
                        ns,
                        "--for=condition=complete",
                        "--timeout=120s",
                    ],
                    capture_output=True,
                )
            except Exception as e:
                print(f"[warn] uninstall job for {instance_name} failed: {e}")

    result = await asyncio.to_thread(
        subprocess.run,
        [
            "kubectl",
            "delete",
            "namespace",
            ns,
            "--ignore-not-found=true",
            "--wait=false",
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise HTTPException(status_code=500, detail=result.stderr.strip())

    return {"ok": True}


@router.get("/apps/{instance_name}/pods")
async def list_pods(instance_name: str):
    result = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "get", "pods", "-n", f"yolab-{instance_name}", "-o", "json"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise HTTPException(status_code=404, detail=result.stderr)
    items = json.loads(result.stdout).get("items", [])
    return [
        {
            "name": p["metadata"]["name"],
            "phase": p["status"].get("phase", "Unknown"),
            "ready": any(
                c["type"] == "Ready" and c["status"] == "True"
                for c in p["status"].get("conditions", [])
            ),
        }
        for p in items
    ]


@router.get("/apps/{instance_name}/describe/{pod_name}")
async def describe_pod(instance_name: str, pod_name: str):
    result = await asyncio.to_thread(
        subprocess.run,
        ["kubectl", "describe", "pod", pod_name, "-n", f"yolab-{instance_name}"],
        capture_output=True,
        text=True,
    )
    return {"output": str(result.stdout) + str(result.stderr)}


@router.get("/apps/{instance_name}/logs/{pod_name}")
async def pod_logs(instance_name: str, pod_name: str):
    async def stream():
        async for line, _ in _stream_proc(
            "kubectl",
            "logs",
            "-n",
            f"yolab-{instance_name}",
            pod_name,
            "--all-containers=true",
            "--follow",
            "--prefix=true",
            "--tail=100",
        ):
            if line:
                yield f"data: {line}\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")
