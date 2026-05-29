import asyncio
import json
import subprocess
import tempfile
from pathlib import Path

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

router = APIRouter()

NAMESPACE = "rook-ceph"
FILESYSTEM = "yolab-cephfs"


def _mgr_exec(*args: str) -> str:
    result = subprocess.run(
        ["kubectl", "exec", "-n", NAMESPACE, "-l", "app=rook-ceph-mgr", "--", *args],
        capture_output=True, text=True, timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    return result.stdout


def _ceph(*args: str) -> dict | list:
    return json.loads(_mgr_exec("ceph", *args, "--format", "json"))


def _kubectl_apply(yaml: str) -> None:
    with tempfile.NamedTemporaryFile(suffix=".yaml", mode="w", delete=False) as f:
        f.write(yaml)
        path = f.name
    try:
        subprocess.run(["kubectl", "apply", "-f", path], capture_output=True, check=True)
    finally:
        Path(path).unlink(missing_ok=True)


def _kubectl_delete(resource: str, name: str, namespace: str = NAMESPACE) -> None:
    cmd = ["kubectl", "delete", resource, name, "--ignore-not-found=true"]
    if namespace:
        cmd += ["-n", namespace]
    subprocess.run(cmd, capture_output=True, check=True)


def _get_filesystem() -> dict:
    result = subprocess.run(
        ["kubectl", "get", "cephfilesystem", FILESYSTEM, "-n", NAMESPACE, "-o", "json"],
        capture_output=True, text=True, check=True,
    )
    return json.loads(result.stdout)


def _patch_filesystem(fs: dict) -> None:
    with tempfile.NamedTemporaryFile(suffix=".json", mode="w", delete=False) as f:
        json.dump(fs, f)
        path = f.name
    try:
        subprocess.run(["kubectl", "apply", "-f", path], capture_output=True, check=True)
    finally:
        Path(path).unlink(missing_ok=True)


# ─── Routes ───────────────────────────────────────────────────────────────────


@router.get("/ceph/status")
async def ceph_status():
    try:
        data = await asyncio.to_thread(_ceph, "status")
        pg = data.get("pgmap", {})
        osd = data.get("osdmap", {})
        return {
            "available": True,
            "health": data.get("health", {}).get("status", "HEALTH_UNKNOWN"),
            "osd_count": osd.get("num_osds", 0),
            "osd_up": osd.get("num_up_osds", 0),
            "total_bytes": pg.get("bytes_total", 0),
            "used_bytes": pg.get("bytes_used", 0),
        }
    except Exception as e:
        return {"available": False, "error": str(e)}


@router.get("/ceph/osds")
async def ceph_osds():
    try:
        data = await asyncio.to_thread(_ceph, "osd", "metadata")
        return [
            {
                "id": osd.get("id"),
                "hostname": osd.get("hostname"),
                "devices": osd.get("devices", ""),
                "size_bytes": int(osd.get("bluestore_bdev_size", 0)),
            }
            for osd in data
        ]
    except Exception:
        return []


class CreatePoolRequest(BaseModel):
    instance_name: str
    redundancy: str = "none"  # "none" | "mirror" | "ec"


@router.post("/ceph/pools")
async def create_pool(body: CreatePoolRequest):
    pool_name = f"yolab-{body.instance_name}-data"
    sc_name = f"yolab-{body.instance_name}"

    if body.redundancy == "mirror":
        pool_spec = {"replicated": {"size": 2, "requireSafeReplicaSize": False}}
    elif body.redundancy == "ec":
        pool_spec = {"erasureCoded": {"codingChunks": 1, "dataChunks": 2}}
    else:
        pool_spec = {"replicated": {"size": 1, "requireSafeReplicaSize": False}}

    # Add the data pool to the CephFilesystem CRD
    def _add_pool():
        fs = _get_filesystem()
        pools = fs["spec"].get("dataPools", [])
        if not any(p["name"] == pool_name for p in pools):
            pools.append({"name": pool_name, **pool_spec})
            fs["spec"]["dataPools"] = pools
            _patch_filesystem(fs)

    await asyncio.to_thread(_add_pool)

    # Create a StorageClass pointing to this pool
    sc_yaml = f"""apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: {sc_name}
provisioner: rook-ceph.cephfs.csi.ceph.com
parameters:
  clusterID: {NAMESPACE}
  fsName: {FILESYSTEM}
  pool: {pool_name}
  csi.storage.k8s.io/provisioner-secret-name: rook-csi-cephfs-provisioner
  csi.storage.k8s.io/provisioner-secret-namespace: {NAMESPACE}
  csi.storage.k8s.io/node-stage-secret-name: rook-csi-cephfs-node
  csi.storage.k8s.io/node-stage-secret-namespace: {NAMESPACE}
allowVolumeExpansion: true
reclaimPolicy: Delete
volumeBindingMode: Immediate
"""
    await asyncio.to_thread(_kubectl_apply, sc_yaml)
    return {"storage_class": sc_name}


@router.delete("/ceph/pools/{name}")
async def delete_pool(name: str):
    await asyncio.to_thread(_kubectl_delete, "storageclass", f"yolab-{name}", "")

    def _remove_pool():
        pool_name = f"yolab-{name}-data"
        try:
            fs = _get_filesystem()
            pools = [p for p in fs["spec"].get("dataPools", []) if p["name"] != pool_name]
            fs["spec"]["dataPools"] = pools
            _patch_filesystem(fs)
        except Exception:
            pass

    await asyncio.to_thread(_remove_pool)
    return {"ok": True}
