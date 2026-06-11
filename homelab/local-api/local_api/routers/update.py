import asyncio
import json
import subprocess

from fastapi import APIRouter, HTTPException
from fastapi.responses import StreamingResponse
from pydantic import BaseModel

from local_api.settings import settings

router = APIRouter()

_CHANNEL_FILE = settings.built_dir / "channel.json"


class Channel(BaseModel):
    remote: str = "origin"
    ref: str = "main"


class RemoteEntry(BaseModel):
    name: str
    url: str


class ChannelInfo(BaseModel):
    remote: str
    ref: str
    remotes: list[RemoteEntry]


def _read_channel() -> Channel:
    try:
        return Channel(**json.loads(_CHANNEL_FILE.read_text()))
    except Exception:
        return Channel()


def _write_channel(ch: Channel) -> None:
    _CHANNEL_FILE.parent.mkdir(parents=True, exist_ok=True)
    _CHANNEL_FILE.write_text(ch.model_dump_json())


def _list_remotes() -> list[RemoteEntry]:
    r = subprocess.run(
        ["git", "-C", settings.yolab_repo_path, "remote", "-v"],
        capture_output=True, text=True,
    )
    seen: set[str] = set()
    remotes: list[RemoteEntry] = []
    for line in r.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 2 and "(fetch)" in line:
            name, url = parts[0], parts[1]
            if name not in seen:
                seen.add(name)
                remotes.append(RemoteEntry(name=name, url=url))
    return remotes


@router.get("/update/channel", response_model=ChannelInfo)
async def get_channel() -> ChannelInfo:
    ch = _read_channel()
    remotes = await asyncio.to_thread(_list_remotes)
    return ChannelInfo(remote=ch.remote, ref=ch.ref, remotes=remotes)


@router.put("/update/channel", response_model=Channel)
async def set_channel(ch: Channel) -> Channel:
    _write_channel(ch)
    return ch


@router.post("/update/remotes", response_model=RemoteEntry)
async def add_remote(entry: RemoteEntry) -> RemoteEntry:
    r = subprocess.run(
        ["git", "-C", settings.yolab_repo_path, "remote", "add", entry.name, entry.url],
        capture_output=True, text=True,
    )
    if r.returncode != 0:
        raise HTTPException(status_code=400, detail=r.stderr.strip())
    return entry


@router.delete("/update/remotes/{name}")
async def remove_remote(name: str) -> dict:
    subprocess.run(
        ["git", "-C", settings.yolab_repo_path, "remote", "remove", name],
        capture_output=True,
    )
    return {"ok": True}


def _run_cmd(cmd: list[str]):
    """Yield stdout lines from a subprocess, then yield the return code."""
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    assert proc.stdout is not None
    for line in proc.stdout:
        yield line.rstrip()
    proc.wait()
    yield proc.returncode


@router.post("/update", response_class=StreamingResponse)
async def update() -> StreamingResponse:
    ch = _read_channel()

    async def stream():
        # Fetch from the configured remote. Also fetch tags so tag refs work.
        fetch_cmd = ["git", "-C", settings.yolab_repo_path, "fetch", ch.remote, "--tags"]
        yield f"data: $ {' '.join(fetch_cmd)}\n\n"
        last = None
        for item in _run_cmd(fetch_cmd):
            if isinstance(item, int):
                last = item
            else:
                yield f"data: {item}\n\n"
        if last != 0:
            yield f"data: [ERROR] fetch failed (exit {last})\n\n"
            return

        # Reset to the ref. Try <remote>/<ref> first (works for branches).
        # Fall back to bare <ref> (works for tags and commit hashes).
        reset_target = f"{ch.remote}/{ch.ref}"
        check = subprocess.run(
            ["git", "-C", settings.yolab_repo_path, "rev-parse", "--verify", reset_target],
            capture_output=True,
        )
        if check.returncode != 0:
            reset_target = ch.ref

        reset_cmd = ["git", "-C", settings.yolab_repo_path, "reset", "--hard", reset_target]
        yield f"data: $ {' '.join(reset_cmd)}\n\n"
        last = None
        for item in _run_cmd(reset_cmd):
            if isinstance(item, int):
                last = item
            else:
                yield f"data: {item}\n\n"
        if last != 0:
            yield f"data: [ERROR] reset failed (exit {last})\n\n"
            return

        flake = f"path:{settings.yolab_repo_path}#{settings.yolab_flake_target}"
        yield f"data: $ nixos-rebuild switch --flake {flake} --print-build-logs\n\n"
        yield "data: [INFO] nixos-rebuild launched — service will restart shortly\n\n"

        settings.rebuild_log.parent.mkdir(parents=True, exist_ok=True)
        log_file = open(settings.rebuild_log, "w")
        proc = subprocess.Popen(
            ["nixos-rebuild", "switch", "--flake", flake, "--print-build-logs"],
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=log_file,
            start_new_session=True,
        )
        log_file.close()
        settings.rebuild_pid.write_text(str(proc.pid))

    return StreamingResponse(stream(), media_type="text/event-stream")
