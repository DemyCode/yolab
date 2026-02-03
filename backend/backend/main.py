from contextlib import asynccontextmanager

from fastapi import FastAPI

from backend.database import init_db
from backend.routes import internal, plugin, services, stats, templates, tokens
from backend.settings import settings


@asynccontextmanager
async def lifespan(app: FastAPI):
    """Lifespan event handler for startup and shutdown."""
    # Startup
    init_db()
    yield
    # Shutdown (if needed)


app = FastAPI(title="FRP IPv6 Tunneling Service", lifespan=lifespan)


@app.get("/health")
async def health_check():
    """Health check endpoint for monitoring."""
    return {"status": "healthy", "service": "backend"}


app.include_router(tokens.router)
app.include_router(services.router)
app.include_router(templates.router)
app.include_router(stats.router)
app.include_router(internal.router)
app.include_router(plugin.router)


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(
        app, host=settings.registration_api_host, port=settings.registration_api_port
    )
