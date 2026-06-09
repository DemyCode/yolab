import subprocess

from fastapi import APIRouter
from fastapi.responses import StreamingResponse

from local_api.settings import settings

router = APIRouter()


@router.post("/update", response_class=StreamingResponse)
async def update() -> StreamingResponse:
    async def stream():
        for cmd in [
            ["git", "-C", settings.yolab_repo_path, "fetch", "origin"],
            ["git", "-C", settings.yolab_repo_path, "reset", "--hard", "origin/main"],
        ]:
            yield f"data: $ {' '.join(cmd)}\n\n"
            try:
                proc = subprocess.Popen(
                    cmd,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                )
                assert proc.stdout is not None
                for line in proc.stdout:
                    yield f"data: {line.rstrip()}\n\n"
                proc.wait()
                if proc.returncode != 0:
                    yield f"data: [ERROR] {cmd[2]} failed (exit {proc.returncode})\n\n"
                    return
            except Exception as e:
                yield f"data: [ERROR] {e}\n\n"
                return

        flake = f"path:{settings.yolab_repo_path}#{settings.yolab_flake_target}"
        yield f"data: $ nixos-rebuild switch --flake {flake} --impure --verbose --print-build-logs\n\n"
        yield "data: [INFO] nixos-rebuild launched — service will restart shortly\n\n"

        settings.rebuild_log.parent.mkdir(parents=True, exist_ok=True)
        log_file = open(settings.rebuild_log, "w")
        proc = subprocess.Popen(
            [
                "nixos-rebuild",
                "switch",
                "--flake",
                flake,
                "--impure",
                "--verbose",
                "--print-build-logs",
            ],
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=log_file,
            start_new_session=True,
        )
        log_file.close()
        settings.rebuild_pid.write_text(str(proc.pid))

    return StreamingResponse(stream(), media_type="text/event-stream")
