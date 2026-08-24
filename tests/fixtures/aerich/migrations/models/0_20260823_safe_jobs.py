from tortoise import BaseDBAsyncClient


async def upgrade(db: BaseDBAsyncClient) -> str:
    return """
        CREATE TABLE "jobs" (
            "id" BIGSERIAL NOT NULL PRIMARY KEY,
            "state" VARCHAR(32) NOT NULL
        );
        CREATE INDEX "jobs_state_idx" ON "jobs" ("state");
    """
