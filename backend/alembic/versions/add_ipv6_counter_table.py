"""Add IPv6Counter table for atomic allocation

Revision ID: f7e8d9c4b5a6
Revises: a1b2c3d4e5f6
Create Date: 2026-01-22

"""

import sqlalchemy as sa

from alembic import op

# revision identifiers, used by Alembic.
revision = "f7e8d9c4b5a6"
down_revision = "a1b2c3d4e5f6"
branch_labels = None
depends_on = None


def upgrade() -> None:
    # Create IPv6Counter table
    op.create_table(
        "ipv6_counter",
        sa.Column("id", sa.Integer(), nullable=False),
        sa.Column("counter", sa.Integer(), nullable=False, server_default="0"),
        sa.PrimaryKeyConstraint("id"),
    )

    # Initialize counter with the current max service ID
    # This ensures we don't reuse IPv6 addresses
    op.execute("""
        INSERT INTO ipv6_counter (id, counter)
        SELECT 1, COALESCE(MAX(id), 0)
        FROM services
        ON CONFLICT DO NOTHING
    """)


def downgrade() -> None:
    op.drop_table("ipv6_counter")
