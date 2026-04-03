import subprocess

from fastapi import APIRouter

from local_api.settings import settings

router = APIRouter()


def _git(*args: str) -> str:
    return subprocess.check_output(
        ["git", *args], cwd=settings.yolab_repo_path, text=True
    ).strip()


@router.get("/api/status")
async def status():
    try:
        return {
            "commit_hash": _git("rev-parse", "HEAD"),
            "commit_message": _git("log", "-1", "--pretty=%s"),
            "commit_date": _git("log", "-1", "--pretty=%cI"),
            "platform": settings.yolab_platform,
            "flake_target": settings.yolab_flake_target,
        }
    except Exception as e:
        return {
            "commit_hash": "",
            "commit_message": "",
            "commit_date": "",
            "platform": settings.yolab_platform,
            "flake_target": settings.yolab_flake_target,
            "error": str(e),
        }
