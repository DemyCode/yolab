from pathlib import Path

from alembic.config import Config
from sqlalchemy.pool import QueuePool
from sqlmodel import Session, create_engine

from alembic import command
from backend.settings import settings

engine = create_engine(
    settings.database_url,
    poolclass=QueuePool,
    pool_size=10,
    max_overflow=20,
    pool_pre_ping=True,
    echo=True,
)


def get_db():
    with Session(engine) as session:
        yield session


def init_db():
    alembic_cfg_path = Path(__file__).parent.parent / "alembic.ini"
    alembic_cfg = Config(str(alembic_cfg_path))
    alembic_cfg.set_main_option("sqlalchemy.url", settings.database_url)
    print("Running Alembic migrations...")
    command.upgrade(alembic_cfg, "head")
    print("✓ Database migrations completed successfully")
