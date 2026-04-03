import subprocess

from fastapi import APIRouter
from fastapi.responses import StreamingResponse

from local_api.routers.rebuild import REBUILD_LOG, REBUILD_PID
from local_api.settings import settings

router = APIRouter()


@router.post("/api/update")
async def update():
    async def stream():
        yield f"data: $ git -C {settings.yolab_repo_path} pull\n\n"
        try:
            proc = subprocess.Popen(
                ["git", "-C", settings.yolab_repo_path, "pull"],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            for line in proc.stdout:
                yield f"data: {line.rstrip()}\n\n"
            proc.wait()
            if proc.returncode != 0:
                yield f"data: [ERROR] git pull failed (exit {proc.returncode})\n\n"
                return
        except Exception as e:
            yield f"data: [ERROR] {e}\n\n"
            return

        flake = f"path:{settings.yolab_repo_path}#{settings.yolab_flake_target}"
        yield f"data: $ nixos-rebuild switch --flake {flake} --verbose --print-build-logs\n\n"
        yield "data: [INFO] nixos-rebuild launched — service will restart shortly\n\n"

        REBUILD_LOG.parent.mkdir(parents=True, exist_ok=True)
        log_file = open(REBUILD_LOG, "w")
        proc = subprocess.Popen(
            [
                "nixos-rebuild",
                "switch",
                "--flake",
                flake,
                "--verbose",
                "--print-build-logs",
            ],
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=log_file,
            start_new_session=True,
        )
        log_file.close()
        REBUILD_PID.write_text(str(proc.pid))

    return StreamingResponse(stream(), media_type="text/event-stream")
