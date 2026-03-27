import json
import os
import subprocess
from pathlib import Path
from typing import Any

from node_agent.config import DISK_JSON_NAME, YOLAB_DATA_ROOT

VOLUMES_META_ROOT = "/yolab/volumes"


def _service_by_mount_path() -> dict[str, str]:
    """Read volume metadata and return {disk_mount_path: service_name}."""
    mapping: dict[str, str] = {}
    if not os.path.isdir(VOLUMES_META_ROOT):
        return mapping
    for svc in os.listdir(VOLUMES_META_ROOT):
        svc_dir = os.path.join(VOLUMES_META_ROOT, svc)
        if not os.path.isdir(svc_dir):
            continue
        for fname in os.listdir(svc_dir):
            if not fname.endswith(".json"):
                continue
            try:
                with open(os.path.join(svc_dir, fname)) as f:
                    meta = json.load(f)
                for path in meta.get("disk_paths", []):
                    mapping[path] = svc
            except Exception:
                pass
    return mapping


def _run(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(list(args), capture_output=True, text=True, check=True)


def _disk_json_path(disk_id: str) -> Path:
    return Path(YOLAB_DATA_ROOT) / disk_id / DISK_JSON_NAME


def read_disk_json(disk_id: str) -> dict[str, Any] | None:
    p = _disk_json_path(disk_id)
    if p.exists():
        return json.loads(p.read_text())
    return None


def write_disk_json(disk_id: str, data: dict[str, Any]) -> None:
    p = _disk_json_path(disk_id)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(data, indent=2))


def _statvfs(path: str) -> tuple[int, int] | None:
    try:
        st = os.statvfs(path)
        total = st.f_blocks * st.f_frsize
        free = st.f_bavail * st.f_frsize
        return total, free
    except OSError:
        return None


def _collect_mountpoints(dev: dict[str, Any]) -> set[str]:
    mounts: set[str] = set()
    mp = dev.get("mountpoint") or (dev.get("mountpoints") or [None])[0]
    if mp:
        mounts.add(mp)
    for child in dev.get("children", []):
        mounts |= _collect_mountpoints(child)
    return mounts


def discover_disks() -> list[dict[str, Any]]:
    disks: list[dict[str, Any]] = []
    service_by_mount = _service_by_mount_path()

    try:
        result = _run("lsblk", "-J", "-o", "NAME,SIZE,MOUNTPOINT,TYPE")
        lsblk_data = json.loads(result.stdout)
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        lsblk_data = {"blockdevices": []}

    system_names: set[str] = set()
    for dev in lsblk_data.get("blockdevices", []):
        if dev.get("type") != "disk":
            continue
        if "/" in _collect_mountpoints(dev):
            system_names.add(dev["name"])

    # Include the root/system disk(s) with a "system" status so the UI can show OS disk usage.
    for dev in lsblk_data.get("blockdevices", []):
        if dev.get("type") != "disk" or dev["name"] not in system_names:
            continue
        sizes = _statvfs("/")
        disks.append({
            "disk_id": dev["name"],
            "device": f"/dev/{dev['name']}",
            "label": "System",
            "type": "block",
            "mount_path": "/",
            "status": "system",
            "service_name": None,
            "total_bytes": sizes[0] if sizes else None,
            "free_bytes": sizes[1] if sizes else None,
            "data_written": True,
            "disk_json": None,
        })

    for dev in lsblk_data.get("blockdevices", []):
        if dev.get("type") != "disk" or dev["name"] in system_names:
            continue
        all_mounts = _collect_mountpoints(dev)
        if "[SWAP]" in all_mounts:
            continue
        device_path = f"/dev/{dev['name']}"
        if not all_mounts:
            disks.append({
                "disk_id": dev["name"],
                "device": device_path,
                "label": dev.get("size"),
                "type": "block",
                "mount_path": None,
                "status": "unformatted",
                "service_name": None,
                "total_bytes": None,
                "free_bytes": None,
                "data_written": False,
                "disk_json": None,
            })
            continue
        mount = next(iter(all_mounts))
        disk_id_candidate = os.path.basename(mount)
        data = read_disk_json(disk_id_candidate) if mount.startswith(YOLAB_DATA_ROOT) else None
        sizes = _statvfs(mount)
        disks.append({
            "disk_id": data["disk_id"] if data else dev["name"],
            "device": device_path,
            "label": data.get("label") if data else dev.get("size"),
            "type": "block",
            "mount_path": mount,
            "status": "registered" if data else "incompatible",
            "service_name": service_by_mount.get(mount),
            "total_bytes": sizes[0] if sizes else None,
            "free_bytes": sizes[1] if sizes else None,
            "data_written": data.get("data_written", False) if data else False,
            "disk_json": data,
        })

    try:
        with open("/proc/mounts") as f:
            mounts_text = f.read()
    except OSError:
        mounts_text = ""

    net_fstypes = {"cifs", "nfs", "nfs4"}
    for line in mounts_text.splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        source, mountpoint, fstype = parts[0], parts[1], parts[2]
        if fstype not in net_fstypes:
            continue
        disk_id_candidate = os.path.basename(mountpoint)
        data = read_disk_json(disk_id_candidate) if mountpoint.startswith(YOLAB_DATA_ROOT) else None
        sizes = _statvfs(mountpoint)
        disks.append({
            "disk_id": data["disk_id"] if data else disk_id_candidate,
            "device": source,
            "label": data.get("label") if data else source,
            "type": "network",
            "mount_path": mountpoint,
            "status": "registered" if data else "unconfigured_network",
            "service_name": service_by_mount.get(mountpoint),
            "total_bytes": sizes[0] if sizes else None,
            "free_bytes": sizes[1] if sizes else None,
            "data_written": data.get("data_written", False) if data else False,
            "disk_json": data,
        })

    for entry in _discover_directory_disks():
        disks.append(entry)

    return disks


def _discover_directory_disks() -> list[dict[str, Any]]:
    results = []
    candidates: list[str] = []

    for base in ["/mnt/c", "/mnt/d", "/mnt/e", "/mnt/f"]:
        if os.path.isdir(base):
            candidates.append(base)

    for vol in _list_macos_volumes():
        candidates.append(vol)

    for base in candidates:
        yolab_dir = os.path.join(base, "yolab-data")
        if not os.path.isdir(yolab_dir):
            continue
        for entry in os.listdir(yolab_dir):
            full = os.path.join(yolab_dir, entry)
            if not os.path.isdir(full):
                continue
            data = _read_disk_json_at(full)
            sizes = _statvfs(full)
            results.append({
                "disk_id": data["disk_id"] if data else entry,
                "device": base,
                "label": data.get("label") if data else base,
                "type": "directory",
                "mount_path": full,
                "status": "registered" if data else "incompatible",
                "total_bytes": sizes[0] if sizes else None,
                "free_bytes": sizes[1] if sizes else None,
                "data_written": data.get("data_written", False) if data else False,
                "disk_json": data,
            })
    return results


def _list_macos_volumes() -> list[str]:
    vols_dir = "/Volumes"
    if not os.path.isdir(vols_dir):
        return []
    return [
        os.path.join(vols_dir, v)
        for v in os.listdir(vols_dir)
        if os.path.isdir(os.path.join(vols_dir, v)) and v not in ("Macintosh HD",)
    ]


def _read_disk_json_at(directory: str) -> dict[str, Any] | None:
    p = Path(directory) / "yolab" / "disk.json"
    if p.exists():
        return json.loads(p.read_text())
    return None


def init_block_disk(disk_id: str, device: str, label: str | None) -> None:
    mount_path = f"{YOLAB_DATA_ROOT}/{disk_id}"
    os.makedirs(mount_path, exist_ok=True)
    _run("mkfs.ext4", "-F", device)
    _run("mount", device, mount_path)
    write_disk_json(disk_id, {
        "disk_id": disk_id,
        "device": device,
        "label": label,
        "type": "block",
        "mount_path": mount_path,
        "data_written": False,
    })


def init_directory_disk(disk_id: str, base_path: str, label: str | None) -> str:
    mount_path = os.path.join(base_path, "yolab-data", disk_id)
    os.makedirs(mount_path, exist_ok=True)
    meta_dir = os.path.join(mount_path, "yolab")
    os.makedirs(meta_dir, exist_ok=True)
    data = {
        "disk_id": disk_id,
        "device": base_path,
        "label": label,
        "type": "directory",
        "mount_path": mount_path,
        "data_written": False,
    }
    Path(os.path.join(meta_dir, "disk.json")).write_text(json.dumps(data, indent=2))
    return mount_path


def init_network_disk(disk_id: str, mount_path: str, label: str | None) -> None:
    write_disk_json(disk_id, {
        "disk_id": disk_id,
        "device": None,
        "label": label,
        "type": "network",
        "mount_path": mount_path,
        "data_written": False,
    })


def mark_data_written(disk_id: str) -> None:
    data = read_disk_json(disk_id)
    if data and not data.get("data_written"):
        data["data_written"] = True
        write_disk_json(disk_id, data)
