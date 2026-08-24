from typing import Any, Awaitable, Callable

from tortoise import BaseDBAsyncClient


async def execute_statement(connection: Any, statement: str) -> None:
    await connection.execute(statement)


async def run_with_lock_timeout(
    db: BaseDBAsyncClient, operation: Callable[[Any], Awaitable[None]]
) -> None:
    async with db.acquire_connection() as connection:
        await operation(connection)


async def _upgrade_attempt(connection: Any) -> None:
    await execute_statement(
        connection,
        'CREATE INDEX "jobs_state_idx" ON "jobs" ("state");',
    )
    await execute_statement(
        connection,
        'CREATE INDEX CONCURRENTLY "jobs_owner_idx" ON "jobs" ("owner_id");',
    )
    await execute_statement(connection, 'DROP INDEX "jobs_old_state_idx";')
    await execute_statement(connection, 'ALTER TABLE "jobs" DROP COLUMN "legacy_state";')
    await execute_statement(
        connection,
        'ALTER TABLE "jobs" RENAME COLUMN "old_state" TO "state";',
    )
    await execute_statement(
        connection,
        'ALTER TABLE "jobs" ALTER COLUMN "state" TYPE VARCHAR(64);',
    )
    await execute_statement(
        connection,
        'ALTER TABLE "jobs" ADD COLUMN "owner_id" UUID NOT NULL REFERENCES "owner" ("id");',
    )
    await execute_statement(
        connection,
        'ALTER TABLE "jobs" ADD CONSTRAINT "jobs_state_unique" UNIQUE ("state");',
    )
    await execute_statement(
        connection,
        'ALTER TABLE "jobs" ADD CONSTRAINT "jobs_state_check" CHECK ("state" <> \'\');',
    )
    await execute_statement(
        connection,
        'ALTER TABLE "jobs" ADD CONSTRAINT "jobs_owner_fk" FOREIGN KEY ("owner_id") REFERENCES "owner" ("id");',
    )
    await execute_statement(
        connection,
        """DO $$ BEGIN
            ALTER TABLE "jobs" ADD CONSTRAINT "jobs_safe_fk"
            FOREIGN KEY ("owner_id") REFERENCES "owner" ("id") NOT VALID;
        EXCEPTION WHEN duplicate_object THEN NULL;
        END $$""",
    )


async def upgrade(db: BaseDBAsyncClient) -> str:
    await run_with_lock_timeout(db, _upgrade_attempt)
