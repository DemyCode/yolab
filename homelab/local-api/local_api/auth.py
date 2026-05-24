import secrets
import tomllib
from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.responses import JSONResponse as StarletteJSONResponse
from starlette.types import ASGIApp, Receive, Scope, Send

from local_api.settings import settings

router = APIRouter()

SESSIONS: set[str] = set()


def _get_password() -> str:
    try:
        cfg = tomllib.loads(Path(settings.yolab_config).read_text())
        return cfg["homelab"].get("web_password", "")
    except Exception:
        return ""


class LoginRequest(BaseModel):
    password: str


@router.post("/api/login")
async def login(body: LoginRequest):
    password = _get_password()
    if password and body.password != password:
        return JSONResponse(status_code=401, content={"detail": "Wrong password"})
    token = secrets.token_hex(32)
    SESSIONS.add(token)
    response = JSONResponse({"ok": True})
    response.set_cookie(
        "yolab_session",
        token,
        httponly=True,
        secure=True,
        samesite="strict",
        max_age=86400 * 30,
        path="/",
    )
    return response


@router.post("/api/logout")
async def logout():
    response = JSONResponse({"ok": True})
    response.delete_cookie("yolab_session", path="/")
    return response


class AuthMiddleware:
    def __init__(self, app: ASGIApp) -> None:
        self.app = app

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] not in ("http", "websocket"):
            await self.app(scope, receive, send)
            return

        path = scope.get("path", "")

        # Login endpoint is always public
        if path == "/api/login":
            await self.app(scope, receive, send)
            return

        # If no password is configured, auth is disabled
        if not _get_password():
            await self.app(scope, receive, send)
            return

        # Parse cookies
        headers = {k: v for k, v in scope.get("headers", [])}
        cookie_str = headers.get(b"cookie", b"").decode(errors="replace")
        cookies: dict[str, str] = {}
        for part in cookie_str.split(";"):
            part = part.strip()
            if "=" in part:
                k, v = part.split("=", 1)
                cookies[k.strip()] = v.strip()

        token = cookies.get("yolab_session", "")
        if not token or token not in SESSIONS:
            resp = StarletteJSONResponse(
                status_code=401, content={"detail": "Unauthorized"}
            )
            await resp(scope, receive, send)
            return

        await self.app(scope, receive, send)
