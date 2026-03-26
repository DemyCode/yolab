import json
import logging
import subprocess
from pathlib import Path

log = logging.getLogger(__name__)


def _run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    log.debug("RUN: %s", " ".join(cmd))
    result = subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    if result.stdout.strip():
        log.debug("STDOUT: %s", result.stdout.strip())
    if result.stderr.strip():
        log.debug("STDERR: %s", result.stderr.strip())
    log.debug("EXIT: %d", result.returncode)
    return result


def test_internet() -> bool:
    try:
        result = _run(["ping", "-c", "1", "-W", "2", "1.1.1.1"], timeout=3)
        return result.returncode == 0
    except (subprocess.TimeoutExpired, OSError):
        return False


def _parse_size(size_str: str) -> float:
    size_str = size_str.strip().upper()
    if size_str.endswith("T"):
        return float(size_str[:-1]) * 1000
    if size_str.endswith("G"):
        return float(size_str[:-1])
    if size_str.endswith("M"):
        return float(size_str[:-1]) / 1000
    return 0.0


def _is_removable(name: str) -> bool:
    try:
        return Path(f"/sys/block/{name}/removable").read_text().strip() == "1"
    except OSError:
        return False


def _is_mounted(device: dict) -> bool:
    # util-linux < 2.37 uses "mountpoint" (string), >= 2.37 uses "mountpoints" (list)
    mp = device.get("mountpoint") or device.get("mountpoints")
    if mp:
        return True
    return False


def _has_mounted_children(device: dict) -> bool:
    for child in device.get("children", []):
        if _is_mounted(child):
            return True
        if _has_mounted_children(child):
            return True
    return False


def detect_disks() -> list[dict]:
    result = _run(["lsblk", "-J", "-o", "NAME,SIZE,TYPE,MOUNTPOINTS,TRAN"], timeout=10)
    if result.returncode != 0:
        # Fall back to older column name
        result = _run(["lsblk", "-J", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,TRAN"], timeout=10)
        if result.returncode != 0:
            raise RuntimeError(result.stderr or "lsblk failed")
    data = json.loads(result.stdout)
    disks = []
    for device in data.get("blockdevices", []):
        if device.get("type") != "disk":
            continue
        name = device["name"]
        tran = (device.get("tran") or "").lower()
        is_usb = tran == "usb" or _is_removable(name)
        mounted = _is_mounted(device) or _has_mounted_children(device)
        disks.append(
            {
                "name": f"/dev/{name}",
                "size": device["size"],
                "tran": tran or "unknown",
                "is_usb": is_usb,
                "mounted": mounted,
            }
        )

    internal_available = [d for d in disks if not d["is_usb"] and not d["mounted"]]
    if internal_available:
        recommended = max(internal_available, key=lambda d: _parse_size(d["size"]))
        recommended["recommended"] = True

    return disks
