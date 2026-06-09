from __future__ import annotations

from enum import Enum

from pydantic import BaseModel


class DiskState(str, Enum):
    SYSTEM = "system"
    UNFORMATTED = "unformatted"
    WAITING = "waiting"
    ACTIVE = "active"
    EJECTING = "ejecting"


class DiskInfo(BaseModel):
    name: str
    model: str
    size_bytes: int
    host: str
    hostname: str
    state: DiskState
    ceph_osd_id: int | None = None
    used_bytes: int | None = None
    free_bytes: int | None = None
    can_eject: bool = False
    queue_position: int | None = None
    fs_type: str | None = None


class AddToStorageRequest(BaseModel):
    disk_name: str
    host: str


class EjectRequest(BaseModel):
    disk_name: str
    host: str


class EjectStatus(BaseModel):
    pg_count: int
    done: bool
    safe_to_unplug: bool


class SystemOsdInfo(BaseModel):
    exists: bool
    size_bytes: int | None = None
    fs_free_bytes: int
    ceph_osd_id: int | None = None


class SystemOsdResize(BaseModel):
    size: str


class SystemOsdResizeResponse(BaseModel):
    ok: bool = True
    operation: str  # "extended" | "shrunk" | "unchanged"
