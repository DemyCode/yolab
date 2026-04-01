import subprocess

import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import StreamingResponse

from local_api.settings import settings

app = FastAPI()
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)


def _git(*args: str) -> str:
    return subprocess.check_output(
        ["git", *args], cwd=settings.yolab_repo_path, text=True
    ).strip()


@app.get("/api/status")
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


@app.post("/api/update")
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

        subprocess.Popen(
            [
                "nixos-rebuild",
                "switch",
                "--flake",
                flake,
                "--verbose",
                "--print-build-logs",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )

    return StreamingResponse(stream(), media_type="text/event-stream")


def run():
    uvicorn.run(app, host="0.0.0.0", port=settings.port)
