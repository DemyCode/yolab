import enum
from datetime import datetime, timezone
from typing import List, Optional

from sqlmodel import Field, Relationship, SQLModel


class ServiceType(str, enum.Enum):
    tcp = "tcp"
    udp = "udp"


class User(SQLModel, table=True):
    __tablename__ = "users"

    id: Optional[int] = Field(default=None, primary_key=True)
    account_token: str = Field(unique=True, index=True)

    services: List["Service"] = Relationship(back_populates="user")


class Service(SQLModel, table=True):
    __tablename__ = "services"

    id: Optional[int] = Field(default=None, primary_key=True)

    user_id: int = Field(foreign_key="users.id")
    service_name: str
    service_type: ServiceType
    sub_ipv6: str = Field(unique=True, index=True)
    client_port: int
    frps_internal_port: int = Field(unique=True)

    user: Optional[User] = Relationship(back_populates="services")
