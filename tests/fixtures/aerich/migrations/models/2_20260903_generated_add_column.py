MODELS_STATE = {}
RUN_IN_TRANSACTION = False


async def upgrade(db):
    return 'ALTER TABLE "jobs" ADD "column" TEXT NOT NULL;'
