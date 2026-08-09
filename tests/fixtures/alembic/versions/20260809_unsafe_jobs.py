from alembic import op


def upgrade():
    op.create_index("jobs_state_idx", "jobs", ["state"])
    op.drop_index("old_jobs_idx", table_name="jobs")
    op.create_foreign_key("jobs_owner_fk", "jobs", "owners", ["owner_id"], ["id"])
    op.create_check_constraint("jobs_state_check", "jobs", "state <> ''")
    op.create_exclude_constraint("jobs_overlap_excl", "jobs", "period WITH &&")
    op.alter_column("jobs", "owner_id", nullable=False)
    op.alter_column("jobs", "state", new_column_name="status")
    op.drop_column("jobs", "legacy_state")
    op.execute("CREATE INDEX jobs_created_idx ON jobs (created_at)")
    op.create_index("jobs_owner_idx", "jobs", ["owner_id"], postgresql_concurrently=True)
