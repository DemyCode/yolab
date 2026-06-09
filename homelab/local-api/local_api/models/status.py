from pydantic import BaseModel


class StatusInfo(BaseModel):
    commit_hash: str
    commit_message: str
    commit_date: str
    platform: str
    flake_target: str
    error: str | None = None
