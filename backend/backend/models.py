import enum
from typing import List, Optional

from sqlmodel import Field, Relationship, SQLModel


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
    sub_ipv6: str = Field(unique=True, index=True)
    wg_public_key: str = Field(unique=True)

    user: Optional[User] = Relationship(back_populates="services")
