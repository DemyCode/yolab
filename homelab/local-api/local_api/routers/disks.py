import asyncio
import errno
import json
import subprocess
from pathlib import Path

import httpx
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from local_api import kubectl
from local_api.settings import settings

router = APIRouter()

LINUX_NATIVE_FS = {"ext4", "ext3", "ext2", "xfs", "btrfs", "f2fs"}
MOUNTABLE_FS = LINUX_NATIVE_FS | {"ntfs", "ntfs-3g", "vfat", "exfat"}

# Per-disk state from the last auto-enable attempt: {disk_name: (state, error)}
_disk_states: dict[str, tuple[str, str | None]] = {}


# ── Detection ─────────────────────────────────────────────────────────────────


def _lsblk() -> list[dict]:
    out = subprocess.check_output(
        ["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE"],
        text=True,
    )
    return json.loads(out)["blockdevices"]


def _is_system_disk(device: dict) -> bool:
    for child in device.get("children") or []:
        mp = child.get("mountpoint") or ""
        fstype = child.get("fstype") or ""
        if mp == "/" or mp.startswith("/boot") or mp.startswith("/nix"):
            return True
        if fstype == "LVM2_member":
            return True
        if _is_system_disk(child):
            return True
    return False


def _find_partition(device: dict) -> dict | None:
    """Find first partition (or whole disk) with a mountable FS, skipping system mounts."""
    fstype = device.get("fstype") or ""
    mountpoint = device.get("mountpoint") or ""
    if fstype in MOUNTABLE_FS and (not mountpoint or mountpoint.startswith("/mnt/")):
        return device
    for child in device.get("children") or []:
        result = _find_partition(child)
        if result:
            return result
    return None


def _find_format_target(device: dict) -> str:
    """Return device to format: first partition if any, otherwise the whole disk."""
    for child in device.get("children") or []:
        if child.get("type") == "part":
            return child["name"]
    return device["name"]


# ── NFS exports ───────────────────────────────────────────────────────────────


def _exported_paths() -> set[str]:
    try:
        out = subprocess.run(["exportfs", "-v"], capture_output=True, text=True).stdout
        return {
            line.split()[0]
            for line in out.splitlines()
            if line.split() and line.split()[0].startswith("/")
        }
    except Exception:
        return set()


def _read_exports() -> set[str]:
    if not settings.exports_file.exists():
        return set()
    return {
        line.split()[0]
        for line in settings.exports_file.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    }


def _write_exports(paths: set[str]) -> None:
    settings.exports_file.parent.mkdir(parents=True, exist_ok=True)
    settings.exports_file.write_text(
        "\n".join(
            f"{p} *(rw,sync,no_subtree_check,no_root_squash)" for p in sorted(paths)
        )
        + "\n"
    )


def _export(path: str) -> None:
    paths = _read_exports()
    if path in paths:
        return
    paths.add(path)
    _write_exports(paths)
    subprocess.run(["exportfs", "-ra"], check=True)


# ── Mounting ──────────────────────────────────────────────────────────────────


def _ensure_ntfs_writable(partition: dict, mount_path: str) -> tuple[bool, str]:
    """If an NTFS partition is mounted read-only, unmount and remount read-write."""
    ro_check = subprocess.run(
        ["findmnt", "-n", "-o", "OPTIONS", mount_path],
        capture_output=True,
        text=True,
    )
    opts = ro_check.stdout.strip()
    is_ro = opts.startswith("ro") or ",ro," in opts or opts.endswith(",ro")
    if not is_ro:
        return True, ""
    dev = f"/dev/{partition['name']}"
    subprocess.run(["umount", mount_path], capture_output=True)
    subprocess.run(["ntfsfix", dev], capture_output=True)
    result = subprocess.run(
        ["mount", "-t", "ntfs-3g", "-o", "remove_hiberfile", dev, mount_path],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return False, f"Failed to remount NTFS read-write: {result.stderr.strip()}"
    return True, ""


def _try_mount(partition: dict) -> tuple[bool, str, str]:
    """Mount a partition if not already mounted. Returns (success, error, mount_path)."""
    dev = f"/dev/{partition['name']}"
    fstype = partition.get("fstype") or ""
    mount_path = partition.get("mountpoint") or f"/mnt/{partition['name']}"
    Path(mount_path).mkdir(parents=True, exist_ok=True)

    if partition.get("mountpoint"):
        if fstype in ("ntfs", "ntfs-3g"):
            ok, err = _ensure_ntfs_writable(partition, mount_path)
            return ok, err, mount_path
        return True, "", mount_path

    if fstype in ("ntfs", "ntfs-3g"):
        subprocess.run(["ntfsfix", dev], capture_output=True)
        cmd = ["mount", "-t", "ntfs-3g", "-o", "remove_hiberfile", dev, mount_path]
    else:
        cmd = ["mount", dev, mount_path]

    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        return False, result.stderr.strip(), ""
    return True, "", mount_path


# ── Enable logic ──────────────────────────────────────────────────────────────


def _enable_disk(device: dict) -> tuple[str, str | None]:
    """Bring a disk online: mount it and export via NFS.
    Returns (state, error) where state is 'system', 'ready', 'needs_format', or 'error'."""
    if _is_system_disk(device):
        path = settings.system_storage_path
        Path(path).mkdir(parents=True, exist_ok=True)
        try:
            _export(path)
        except Exception as e:
            return "error", str(e)
        return "system", None

    partition = _find_partition(device)
    if partition is None:
        return "needs_format", "No recognized filesystem — format to use this disk"

    ok, err, mount_path = _try_mount(partition)
    if not ok:
        return "needs_format", err or "Mount failed"

    try:
        Path(mount_path, "yolab").mkdir(exist_ok=True)
    except OSError as e:
        if e.errno == errno.EROFS:
            return "needs_format", f"Disk at {mount_path} is read-only (Windows Fast Startup?)"
        return "error", str(e)

    try:
        _export(mount_path)
    except Exception as e:
        return "error", str(e)

    return "ready", None


# ── Background auto-enable loop ───────────────────────────────────────────────


async def auto_enable_loop() -> None:
    while True:
        try:
            devices = await asyncio.to_thread(_lsblk)
            for d in devices:
                if d.get("type") != "disk":
                    continue
                state, err = await asyncio.to_thread(_enable_disk, d)
                _disk_states[d["name"]] = (state, err)
        except Exception:
            pass
        await asyncio.sleep(30)


# ── Usage ─────────────────────────────────────────────────────────────────────


def _disk_usage(path: str, scan_root: bool = False) -> dict:
    """Return filesystem stats and per-app subdirectory usage.
    scan_root=True: scan path directly (system disk, fully yolab-dedicated).
    scan_root=False: scan path/yolab/ (external disk with mixed content)."""
    out: dict = {"fs_size_bytes": 0, "fs_used_bytes": 0, "app_usage": []}
    p = Path(path)
    if not p.exists():
        return out
    try:
        lines = subprocess.check_output(
            ["df", "-B1", "--output=size,used", path], text=True
        ).splitlines()
        if len(lines) >= 2:
            parts = lines[1].split()
            out["fs_size_bytes"] = int(parts[0])
            out["fs_used_bytes"] = int(parts[1])
    except Exception:
        pass
    scan_dir = p if scan_root else p / "yolab"
    if scan_dir.is_dir():
        try:
            subdirs = [d for d in scan_dir.iterdir() if d.is_dir()]
            if subdirs:
                du = subprocess.check_output(
                    ["du", "-sb"] + [str(d) for d in subdirs], text=True
                )
                out["app_usage"] = [
                    {
                        "name": Path(line.split("\t", 1)[1]).name,
                        "bytes": int(line.split("\t", 1)[0]),
                    }
                    for line in du.splitlines()
                    if "\t" in line
                ]
        except Exception:
            pass
    return out


def _build_disk_info(devices: list[dict]) -> list[dict]:
    out = []
    for d in devices:
        if d.get("type") != "disk":
            continue
        name = d["name"]
        state, error = _disk_states.get(name, ("unknown", None))

        if state == "system":
            storage_path = settings.system_storage_path
            usage = _disk_usage(storage_path, scan_root=True)
        elif state == "ready":
            partition = _find_partition(d)
            if partition:
                storage_path = partition.get("mountpoint") or f"/mnt/{partition['name']}"
                usage = _disk_usage(storage_path)
            else:
                storage_path = None
                usage = {"fs_size_bytes": 0, "fs_used_bytes": 0, "app_usage": []}
        else:
            storage_path = None
            usage = {"fs_size_bytes": 0, "fs_used_bytes": 0, "app_usage": []}

        out.append(
            {
                "name": name,
                "model": (d.get("model") or "").strip(),
                "size_bytes": int(d.get("size") or 0),
                "host": settings.yolab_node_ipv6,
                "state": state,
                "error": error,
                "storage_path": storage_path,
                "fs_size_bytes": usage["fs_size_bytes"],
                "fs_used_bytes": usage["fs_used_bytes"],
                "app_usage": usage["app_usage"],
            }
        )
    return out


# ── Multi-node proxying ───────────────────────────────────────────────────────


def _node_ips() -> list[str]:
    ips = {settings.yolab_node_ipv6}
    try:
        for node in kubectl.get_nodes():
            for addr in node["status"]["addresses"]:
                if ":" in addr["address"]:
                    ips.add(addr["address"])
    except Exception:
        pass
    return list(ips)


async def _gather_from_nodes(path: str) -> list[tuple[str, list]]:
    ips = await asyncio.to_thread(_node_ips)
    async with httpx.AsyncClient(timeout=10) as client:
        results = await asyncio.gather(
            *[client.get(f"http://[{ip}]:{settings.port}{path}") for ip in ips],
            return_exceptions=True,
        )
    return [
        (ip, r.json())
        for ip, r in zip(ips, results)
        if isinstance(r, httpx.Response) and r.status_code == 200
    ]


async def _proxy_post(host: str, path: str, body: dict):
    async with httpx.AsyncClient(timeout=60) as client:
        r = await client.post(
            f"http://[{host}]:{settings.port}{path}",
            json=body,
        )
    if r.status_code != 200:
        raise HTTPException(status_code=r.status_code, detail=r.json().get("detail", "Failed"))
    return r.json()


# ── Routes ────────────────────────────────────────────────────────────────────


@router.get("/api/disks/local")
async def disks_local():
    devices = await asyncio.to_thread(_lsblk)
    return await asyncio.to_thread(_build_disk_info, devices)


@router.get("/api/disks")
async def disks():
    return [
        disk
        for _, disks in await _gather_from_nodes("/api/disks/local")
        for disk in disks
    ]


@router.get("/api/storage/local")
async def storage_local():
    paths = await asyncio.to_thread(_exported_paths)
    return [{"host": settings.yolab_node_ipv6, "path": p} for p in paths]


@router.get("/api/storage")
async def storage():
    return [
        entry
        for _ip, entries in await _gather_from_nodes("/api/storage/local")
        for entry in entries
    ]


class FormatRequest(BaseModel):
    host: str


@router.post("/api/disks/{name}/format")
async def format_disk(name: str, body: FormatRequest):
    if body.host != settings.yolab_node_ipv6:
        return await _proxy_post(body.host, f"/api/disks/{name}/format", body.model_dump())

    def do_format():
        devices = _lsblk()
        device = next((d for d in devices if d["name"] == name), None)
        if device is None:
            return None, "Disk not found"
        target = _find_format_target(device)
        result = subprocess.run(
            ["mkfs.ext4", "-F", f"/dev/{target}"],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            return None, result.stderr.strip()
        # Re-read lsblk so _enable_disk sees the new fstype
        devices = _lsblk()
        return next((d for d in devices if d["name"] == name), None), None

    device, err = await asyncio.to_thread(do_format)
    if err:
        raise HTTPException(status_code=404 if err == "Disk not found" else 500, detail=err)

    if device is not None:
        state, err = await asyncio.to_thread(_enable_disk, device)
        _disk_states[name] = (state, err)

    return {"ok": True}
