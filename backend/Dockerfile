FROM python:3.11-slim

WORKDIR /app

RUN apt-get update && apt-get install -y curl && rm -rf /var/lib/apt/lists/*

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv

ENV UV_SYSTEM_PYTHON=1

COPY pyproject.toml uv.lock ./

RUN uv sync --frozen --no-dev

COPY backend ./backend
COPY alembic ./alembic
COPY alembic.ini .
COPY services ./services

EXPOSE 5000

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD curl -f http://localhost:5000/health || exit 1

CMD ["uv", "run", "python", "-m", "backend.main"]
