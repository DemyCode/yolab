from contextlib import asynccontextmanager

from fastapi import FastAPI

from backend.database import init_db
from backend.routes import services, users


@asynccontextmanager
async def lifespan(app: FastAPI):
    init_db()
    yield


app = FastAPI(title="YoLab Tunneling Service", lifespan=lifespan)


@app.get("/health")
async def health_check():
    return {"status": "healthy", "service": "backend"}


app.include_router(users.router)
app.include_router(services.router)


if __name__ == "__main__":
    import uvicorn

    from backend.settings import settings

    uvicorn.run(app, host="0.0.0.0", port=settings.port, log_level="debug")
