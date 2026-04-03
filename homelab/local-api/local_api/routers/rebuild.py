from pathlib import Path

from fastapi import APIRouter

from local_api.routers.update import REBUILD_LOG, REBUILD_PID

router = APIRouter()


@router.get("/api/rebuild-log")
async def rebuild_log():
    running = False
    if REBUILD_PID.exists():
        try:
            pid = int(REBUILD_PID.read_text().strip())
            running = Path(f"/proc/{pid}").exists()
        except Exception:
            pass
    log = REBUILD_LOG.read_text().splitlines() if REBUILD_LOG.exists() else []
    return {"running": running, "log": log}
