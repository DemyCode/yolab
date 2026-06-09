import json
import subprocess
from dataclasses import dataclass

_CM_NAME = "yolab-disk-priority"
_CM_NS = "rook-ceph"


@dataclass
class PriorityEntry:
    host: str
    disk_name: str

    def key(self) -> str:
        return f"{self.host}:{self.disk_name}"


def _parse_lines(text: str) -> list[str]:
    return [line.strip() for line in (text or "").strip().splitlines() if line.strip()]


def _read_cm() -> dict[str, str]:
    r = subprocess.run(
        ["kubectl", "get", "configmap", "-n", _CM_NS, _CM_NAME, "-o", "json"],
        capture_output=True, text=True, timeout=10,
    )
    if r.returncode != 0:
        return {}
    try:
        return json.loads(r.stdout).get("data", {}) or {}
    except Exception:
        return {}


def _write_cm(data: dict[str, str]) -> None:
    manifest = {
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": _CM_NAME, "namespace": _CM_NS},
        "data": {k: v for k, v in data.items() if v is not None},
    }
    subprocess.run(
        ["kubectl", "apply", "-f", "-"],
        input=json.dumps(manifest),
        capture_output=True, text=True, timeout=10,
        check=True,
    )


def read() -> list[PriorityEntry]:
    data = _read_cm()
    entries = []
    for line in _parse_lines(data.get("priority", "")):
        host, _, disk = line.rpartition(":")
        if host and disk:
            entries.append(PriorityEntry(host=host, disk_name=disk))
    return entries


def write(entries: list[PriorityEntry]) -> None:
    data = _read_cm()
    data["priority"] = "\n".join(e.key() for e in entries)
    _write_cm(data)


def append(host: str, disk_name: str) -> None:
    """Append to end of list if not already present and not rejected."""
    key = f"{host}:{disk_name}"
    data = _read_cm()
    existing = _parse_lines(data.get("priority", ""))
    if key in existing:
        return
    if key in _parse_lines(data.get("rejected", "")):
        return
    existing.append(key)
    data["priority"] = "\n".join(existing)
    _write_cm(data)


def remove(host: str, disk_name: str) -> None:
    """Remove from priority and mark as rejected so it won't auto-append."""
    key = f"{host}:{disk_name}"
    data = _read_cm()
    priority = [l for l in _parse_lines(data.get("priority", "")) if l != key]
    rejected = set(_parse_lines(data.get("rejected", "")))
    rejected.add(key)
    data["priority"] = "\n".join(priority)
    data["rejected"] = "\n".join(sorted(rejected))
    _write_cm(data)


def is_rejected(host: str, disk_name: str) -> bool:
    data = _read_cm()
    return f"{host}:{disk_name}" in _parse_lines(data.get("rejected", ""))


def unreject(host: str, disk_name: str) -> None:
    key = f"{host}:{disk_name}"
    data = _read_cm()
    rejected = set(_parse_lines(data.get("rejected", "")))
    rejected.discard(key)
    data["rejected"] = "\n".join(sorted(rejected))
    _write_cm(data)


def position(host: str, disk_name: str) -> int | None:
    for i, e in enumerate(read(), 1):
        if e.key() == f"{host}:{disk_name}":
            return i
    return None
