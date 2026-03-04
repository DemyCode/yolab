"""wireguard

Revision ID: c3d4e5f6a7b8
Revises: a26048365ab2
Create Date: 2026-03-04 00:00:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
import sqlmodel
from alembic import op

revision: str = "c3d4e5f6a7b8"
down_revision: Union[str, Sequence[str], None] = "a26048365ab2"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    op.drop_column("services", "frps_internal_port")
    op.add_column(
        "services",
        sa.Column("wg_public_key", sqlmodel.sql.sqltypes.AutoString(), nullable=False, server_default=""),
    )
    op.create_index(op.f("ix_services_wg_public_key"), "services", ["wg_public_key"], unique=True)


def downgrade() -> None:
    op.drop_index(op.f("ix_services_wg_public_key"), table_name="services")
    op.drop_column("services", "wg_public_key")
    op.add_column("services", sa.Column("frps_internal_port", sa.INTEGER(), nullable=False, server_default="0"))
    op.create_index(op.f("ix_services_frps_internal_port"), "services", ["frps_internal_port"], unique=True)
