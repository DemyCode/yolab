import json
import subprocess
from datetime import datetime, timezone

from local_api.models.disk import DiskQueue, QueueEntry

_CM_NAME = "yolab-disk-queue"
_CM_NS = "rook-ceph"


def read_queue() -> DiskQueue:
    r = subprocess.run(
        ["kubectl", "get", "configmap", "-n", _CM_NS, _CM_NAME, "-o", "json"],
        capture_output=True, text=True, timeout=10,
    )
    if r.returncode != 0:
        return DiskQueue()
    try:
        cm = json.loads(r.stdout)
        raw = json.loads(cm.get("data", {}).get("queue", "[]"))
        return DiskQueue(entries=[QueueEntry(**e) for e in raw])
    except Exception:
        return DiskQueue()


def write_queue(queue: DiskQueue) -> None:
    payload = json.dumps(
        [e.model_dump(mode="json") for e in queue.entries],
        default=str,
    )
    manifest = {
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": _CM_NAME, "namespace": _CM_NS},
        "data": {"queue": payload},
    }
    subprocess.run(
        ["kubectl", "apply", "-f", "-"],
        input=json.dumps(manifest),
        capture_output=True, text=True, timeout=10,
        check=True,
    )


def enqueue(disk_name: str, host: str) -> None:
    queue = read_queue()
    queue.entries = [e for e in queue.entries
                     if not (e.disk_name == disk_name and e.host == host)]
    queue.entries.append(QueueEntry(
        disk_name=disk_name,
        host=host,
        queued_at=datetime.now(timezone.utc),
    ))
    write_queue(queue)


def remove_entry(disk_name: str, host: str) -> None:
    queue = read_queue()
    queue.entries = [e for e in queue.entries
                     if not (e.disk_name == disk_name and e.host == host)]
    write_queue(queue)


def pop_next() -> QueueEntry | None:
    queue = read_queue()
    if not queue.entries:
        return None
    entry = queue.entries.pop(0)
    write_queue(queue)
    return entry


def peek_next() -> QueueEntry | None:
    queue = read_queue()
    return queue.entries[0] if queue.entries else None
