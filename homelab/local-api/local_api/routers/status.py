import subprocess

from fastapi import APIRouter

from local_api.settings import settings

router = APIRouter()

def _read_built(name: str) -> str:
    try:
        return (settings.built_dir / name).read_text().strip()
    except Exception:
        return ""


def _git(*args: str) -> str:
    return subprocess.check_output(
        ["git", *args], cwd=settings.yolab_repo_path, text=True
    ).strip()


def _built_or_git(filename: str, *git_args: str) -> str:
    val = _read_built(filename)
    if val:
        return val
    return _git(*git_args)


@router.get("/api/status")
async def status():
    try:
        return {
            "commit_hash": _built_or_git("built-hash", "rev-parse", "HEAD"),
            "commit_message": _built_or_git("built-message", "log", "-1", "--pretty=%s"),
            "commit_date": _built_or_git("built-date", "log", "-1", "--pretty=%cI"),
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
