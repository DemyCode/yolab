import ctypes
import ctypes.util
import secrets
import tomllib
from pathlib import Path
from typing import cast

from fastapi import APIRouter
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.responses import JSONResponse as StarletteJSONResponse
from starlette.types import ASGIApp, Receive, Scope, Send

from local_api.models.common import OkResponse
from local_api.settings import settings

router = APIRouter()

SESSIONS: set[str] = set()


def _get_hash() -> str:
    """Return the homelab_password_hash from config.toml, or '' if unset."""
    try:
        cfg = tomllib.loads(Path(settings.yolab_config).read_text())
        return cfg["homelab"].get("homelab_password_hash", "")
    except Exception:
        return ""


def _load_libcrypt() -> ctypes.CDLL:
    """Find libcrypt on any Linux distro including NixOS."""
    import glob

    # Python's own _crypt extension (works in Python ≤ 3.12, uses same libcrypt)
    try:
        import crypt as _crypt  # noqa: PLC0415

        # Wrap it so callers get the same interface as ctypes
        class _PyCrypt:
            def crypt(self, password: bytes, setting: bytes) -> bytes | None:
                result = _crypt.crypt(password.decode(), setting.decode())
                return result.encode() if result else None

        return cast(ctypes.CDLL, _PyCrypt())
    except ImportError:
        pass

    # NixOS: libs are in the current-system profile, not in standard ld paths
    candidates = sorted(glob.glob("/run/current-system/sw/lib/libcrypt.so*"))
    for path in candidates:
        try:
            lib = ctypes.CDLL(path)
            lib.crypt.restype = ctypes.c_char_p
            return lib
        except OSError:
            continue

    # Standard fallback
    for name in [ctypes.util.find_library("crypt"), "libcrypt.so.2", "libcrypt.so.1"]:
        if not name:
            continue
        try:
            lib = ctypes.CDLL(name)
            lib.crypt.restype = ctypes.c_char_p
            return lib
        except OSError:
            continue

    raise OSError("libcrypt not found — cannot verify password hash")


def _verify(password: str, hashed: str) -> bool:
    """Verify a plain-text password against a Linux shadow hash."""
    if not hashed:
        return False
    try:
        lib = _load_libcrypt()
        result = lib.crypt(password.encode(), hashed.encode())
        return result is not None and result.decode() == hashed
    except Exception as e:
        print(f"[auth] password verification error: {e}")
        return False


class LoginRequest(BaseModel):
    password: str


@router.post("/login", response_model=OkResponse)
async def login(body: LoginRequest) -> JSONResponse:
    hashed = _get_hash()
    if hashed and not _verify(body.password, hashed):
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


@router.post("/logout", response_model=OkResponse)
async def logout() -> JSONResponse:
    response = JSONResponse({"ok": True})
    response.delete_cookie("yolab_session", path="/")
    return response


def _is_cluster_internal(scope: Scope) -> bool:
    """True if the request originates from another cluster node.

    Browser requests arrive via Caddy's reverse proxy and appear as ::1.
    Inter-node calls (e.g. _gather_from_nodes) go directly to port 3001
    from the caller's WireGuard private IP (fd00:cafe::/112), which is
    only reachable over the authenticated WireGuard tunnel.
    """
    client = scope.get("client")
    if not client:
        return False
    ip = client[0].lower()
    return ip.startswith("fd") or ip.startswith("fc")


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

        # Inter-node requests come from WireGuard private IPs — already
        # authenticated at the network level, no cookie needed
        if _is_cluster_internal(scope):
            await self.app(scope, receive, send)
            return

        # If no password hash is configured, auth is disabled
        if not _get_hash():
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
