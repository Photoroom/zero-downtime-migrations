from alembic import op


def upgrade():
    op.create_table("jobs")
    op.create_index("jobs_state_idx", "jobs", ["state"])
    op.create_check_constraint("jobs_state_check", "jobs", "state <> ''")
    with op.get_context().autocommit_block():
        op.create_index("jobs_owner_idx", "jobs", ["owner_id"], postgresql_concurrently=True)
        op.execute("CREATE INDEX CONCURRENTLY jobs_created_idx ON jobs (created_at)")
