from pydantic import BaseModel


class OsdUsage(BaseModel):
    osd_id: int
    used_bytes: int
    free_bytes: int
    total_bytes: int
    reweight: float = 1.0


class CephStatus(BaseModel):
    available: bool
    health: str = "HEALTH_UNKNOWN"
    osd_count: int = 0
    osd_up: int = 0
    total_bytes: int = 0
    used_bytes: int = 0
    error: str | None = None
