"""initial schema with port routing

Revision ID: 001_initial_schema
Revises:
Create Date: 2026-02-04 18:20:00.000000

"""

from typing import Sequence, Union

from alembic import op
import sqlalchemy as sa
import sqlmodel

# revision identifiers, used by Alembic.
revision: str = "001_initial_schema"
down_revision: Union[str, Sequence[str], None] = None
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    """Create initial schema with port-based routing."""
    # Create users table
    op.create_table(
        "users",
        sa.Column("id", sa.Integer(), nullable=False),
        sa.Column("account_token", sqlmodel.sql.sqltypes.AutoString(), nullable=False),
        sa.PrimaryKeyConstraint("id"),
    )
    op.create_index(
        op.f("ix_users_account_token"), "users", ["account_token"], unique=True
    )

    # Create services table with new port-based routing fields
    op.create_table(
        "services",
        sa.Column("id", sa.Integer(), nullable=False),
        sa.Column("user_id", sa.Integer(), nullable=False),
        sa.Column("service_name", sqlmodel.sql.sqltypes.AutoString(), nullable=False),
        sa.Column(
            "service_type", sa.Enum("tcp", "udp", name="servicetype"), nullable=False
        ),
        sa.Column("subdomain", sqlmodel.sql.sqltypes.AutoString(), nullable=False),
        sa.Column("sub_ipv6", sqlmodel.sql.sqltypes.AutoString(), nullable=False),
        sa.Column("client_port", sa.Integer(), nullable=False),
        sa.Column("frps_internal_port", sa.Integer(), nullable=False),
        sa.Column("local_port", sa.Integer(), nullable=False),
        sa.ForeignKeyConstraint(
            ["user_id"],
            ["users.id"],
        ),
        sa.PrimaryKeyConstraint("id"),
    )
    op.create_index(
        op.f("ix_services_subdomain"), "services", ["subdomain"], unique=True
    )
    op.create_index(op.f("ix_services_sub_ipv6"), "services", ["sub_ipv6"], unique=True)
    op.create_unique_constraint(
        "uq_services_frps_internal_port", "services", ["frps_internal_port"]
    )


def downgrade() -> None:
    """Drop all tables."""
    op.drop_index(op.f("ix_services_sub_ipv6"), table_name="services")
    op.drop_index(op.f("ix_services_subdomain"), table_name="services")
    op.drop_constraint("uq_services_frps_internal_port", "services", type_="unique")
    op.drop_table("services")
    op.drop_index(op.f("ix_users_account_token"), table_name="users")
    op.drop_table("users")
