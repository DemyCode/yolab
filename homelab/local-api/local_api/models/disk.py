from __future__ import annotations

from pydantic import BaseModel


class DiskItem(BaseModel):
    name: str
    model: str
    size_bytes: int
    host: str
    hostname: str
    is_osd: bool
    is_builtin: bool
    used_bytes: int | None = None
    free_bytes: int | None = None


class DiskOrderEntry(BaseModel):
    host: str
    disk_name: str


class DiskOrderRequest(BaseModel):
    entries: list[DiskOrderEntry]
