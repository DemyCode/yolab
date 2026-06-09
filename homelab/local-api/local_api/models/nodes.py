from pydantic import BaseModel


class NodeInfo(BaseModel):
    name: str
    ip: str
    ready: bool
    roles: list[str]
    joined_at: str


class JoinInfo(BaseModel):
    k3s_token: str
    server_addr: str
