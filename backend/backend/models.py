import enum
from datetime import datetime, timezone
from typing import List, Optional

from sqlmodel import Field, Relationship, SQLModel


class ServiceType(str, enum.Enum):
    tcp = "tcp"
    udp = "udp"


class ServiceStatus(str, enum.Enum):
    active = "active"
    suspended = "suspended"
    deleted = "deleted"


class User(SQLModel, table=True):
    __tablename__ = "users"

    id: Optional[int] = Field(default=None, primary_key=True)
    account_token: str = Field(unique=True, index=True)
    created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))

    services: List["Service"] = Relationship(back_populates="user")


class Service(SQLModel, table=True):
    __tablename__ = "services"

    id: Optional[int] = Field(default=None, primary_key=True)
    user_id: int = Field(foreign_key="users.id")
    service_name: str
    service_type: ServiceType
    subdomain: str = Field(unique=True, index=True)
    ipv6_address: str = Field(unique=True, index=True)
    remote_port: int
    local_port: int
    status: ServiceStatus = Field(default=ServiceStatus.active)
    created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
    last_seen: Optional[datetime] = Field(default=None)

    user: Optional[User] = Relationship(back_populates="services")


class IPv6Counter(SQLModel, table=True):
    __tablename__ = "ipv6_counter"

    id: Optional[int] = Field(default=None, primary_key=True)
    counter: int = Field(default=0)
