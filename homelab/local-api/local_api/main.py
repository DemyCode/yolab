import asyncio
import os
import subprocess

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import StreamingResponse

REPO_PATH = os.environ.get("YOLAB_REPO_PATH", "/etc/nixos")
PLATFORM = os.environ.get("YOLAB_PLATFORM", "nixos")
FLAKE_TARGET = os.environ.get("YOLAB_FLAKE_TARGET", "yolab")


def get_update_commands() -> list[list[str]]:
    rebuild = (
        ["darwin-rebuild", "switch", "--flake", f"{REPO_PATH}#{FLAKE_TARGET}"]
        if PLATFORM == "darwin"
        else ["nixos-rebuild", "switch", "--flake", f"{REPO_PATH}#{FLAKE_TARGET}"]
    )
    return [["git", "-C", REPO_PATH, "pull"], rebuild]


app = FastAPI()
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)


@app.get("/api/health")
def health():
    return {"status": "ok"}


@app.get("/api/status")
def status():
    try:
        result = subprocess.run(
            ["git", "-C", REPO_PATH, "log", "-1", "--format=%H|||%s|||%ci"],
            capture_output=True,
            text=True,
            check=True,
        )
        parts = result.stdout.strip().split("|||")
        return {
            "commit_hash": parts[0] if len(parts) > 0 else "",
            "commit_message": parts[1] if len(parts) > 1 else "",
            "commit_date": parts[2] if len(parts) > 2 else "",
            "platform": PLATFORM,
            "flake_target": FLAKE_TARGET,
        }
    except Exception as e:
        return {"error": str(e), "platform": PLATFORM}


@app.post("/api/update")
async def update():
    async def stream():
        for cmd in get_update_commands():
            yield f"data: $ {' '.join(cmd)}\n\n"
            process = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
            async for line in process.stdout:
                text = line.decode().rstrip()
                if text:
                    yield f"data: {text}\n\n"
            await process.wait()
            if process.returncode != 0:
                yield f"data: [ERROR] exited with code {process.returncode}\n\n"
                return
        yield "data: [DONE]\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")


def run():
    import uvicorn

    uvicorn.run(app, host="127.0.0.1", port=3001)
