"""IPv6-only simplified schema

Revision ID: a1b2c3d4e5f6
Revises: dc745c552096
Create Date: 2026-01-21 22:00:00

"""

import sqlalchemy as sa

from alembic import op

revision = "a1b2c3d4e5f6"
down_revision = "dc745c552096"
branch_labels = None
depends_on = None


def upgrade():
    op.add_column("services", sa.Column("ipv6_address", sa.String(), nullable=True))
    op.create_index(
        op.f("ix_services_ipv6_address"), "services", ["ipv6_address"], unique=True
    )

    op.alter_column("services", "subdomain", existing_type=sa.VARCHAR(), nullable=False)

    op.alter_column(
        "services", "remote_port", existing_type=sa.INTEGER(), nullable=True
    )

    op.execute("""
        DELETE FROM services WHERE service_type IN ('web', 'http', 'https')
    """)


def downgrade():
    op.drop_index(op.f("ix_services_ipv6_address"), table_name="services")
    op.drop_column("services", "ipv6_address")

    op.alter_column("services", "subdomain", existing_type=sa.VARCHAR(), nullable=True)
