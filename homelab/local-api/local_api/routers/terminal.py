import asyncio

from fastapi import APIRouter
from fastapi.responses import StreamingResponse
from pydantic import BaseModel

router = APIRouter()


class ExecRequest(BaseModel):
    command: str


@router.post("/api/terminal/exec")
async def exec_command(req: ExecRequest):
    async def stream():
        try:
            proc = await asyncio.create_subprocess_exec(
                "bash", "-c", req.command,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
            async for line in proc.stdout:
                yield f"data: {line.decode(errors='replace').rstrip()}\n\n"
            await proc.wait()
            yield f"data: [EXIT:{proc.returncode}]\n\n"
        except Exception as e:
            yield f"data: [ERROR] {e}\n\n"
            yield "data: [EXIT:1]\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")
