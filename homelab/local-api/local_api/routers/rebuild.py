from pathlib import Path

from fastapi import APIRouter

from local_api.models.apps import RebuildLog
from local_api.settings import settings

router = APIRouter()


@router.get("/rebuild-log", response_model=RebuildLog)
async def rebuild_log():
    running = False
    if settings.rebuild_pid.exists():
        try:
            pid = int(settings.rebuild_pid.read_text().strip())
            running = Path(f"/proc/{pid}").exists()
        except Exception:
            pass
    try:
        log = (
            settings.rebuild_log.read_text(errors="replace").splitlines()
            if settings.rebuild_log.exists()
            else []
        )
    except Exception:
        log = []
    return RebuildLog(running=running, log=log)
