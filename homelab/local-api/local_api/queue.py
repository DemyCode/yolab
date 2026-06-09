from pathlib import Path

# Each node manages its own queue as plain files.
# File presence = disk is queued. mtime = enqueue order.
_QUEUE_DIR = Path("/var/lib/yolab/disk-queue")


def _entries() -> list[Path]:
    _QUEUE_DIR.mkdir(parents=True, exist_ok=True)
    return sorted(_QUEUE_DIR.iterdir(), key=lambda p: p.stat().st_mtime)


def get_all() -> list[str]:
    return [p.name for p in _entries()]


def is_queued(disk_name: str) -> bool:
    return (_QUEUE_DIR / disk_name).exists()


def position(disk_name: str) -> int | None:
    for i, p in enumerate(_entries(), 1):
        if p.name == disk_name:
            return i
    return None


def enqueue(disk_name: str) -> None:
    _QUEUE_DIR.mkdir(parents=True, exist_ok=True)
    (_QUEUE_DIR / disk_name).touch()


def dequeue(disk_name: str) -> None:
    (_QUEUE_DIR / disk_name).unlink(missing_ok=True)


def peek_next() -> str | None:
    entries = _entries()
    return entries[0].name if entries else None
