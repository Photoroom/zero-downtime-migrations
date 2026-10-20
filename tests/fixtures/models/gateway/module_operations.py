"""Durable ownership for a Module Template authoring operation that provisions outside its lock."""

import secrets
import string

from sqlalchemy import BigInteger, Identity, Index, Integer, String, UniqueConstraint
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import Mapped, mapped_column

from app.persistence.models import Base, TimestampMixin

MODULE_OPERATION_ID_PREFIX = "module_operation"
MODULE_OPERATION_ID_SUFFIX_LENGTH = 21
MODULE_OPERATION_ID_LENGTH = len(MODULE_OPERATION_ID_PREFIX) + 1 + MODULE_OPERATION_ID_SUFFIX_LENGTH
_BASE62 = string.ascii_letters + string.digits


def new_module_operation_public_id() -> str:
    """Create one opaque, typed public Module authoring operation identifier."""
    return f"{MODULE_OPERATION_ID_PREFIX}_{''.join(secrets.choice(_BASE62) for _ in range(MODULE_OPERATION_ID_SUFFIX_LENGTH))}"


class ModuleAuthoringOperation(TimestampMixin, Base):
    """One outstanding authoring operation, so provider calls run with no transaction open and survive process loss."""

    __tablename__ = "ai_module_authoring_operations"
    __table_args__ = (
        UniqueConstraint("public_id", name="uq_ai_module_authoring_operations_public_id"),
        Index("one_operation_per_module_authoring_identity", "module_id", unique=True),
    )

    internal_id: Mapped[int] = mapped_column(BigInteger, Identity(), primary_key=True)
    public_id: Mapped[str] = mapped_column(String(MODULE_OPERATION_ID_LENGTH), nullable=False)
    module_id: Mapped[str] = mapped_column(String(255), nullable=False)
    version: Mapped[int] = mapped_column(Integer, nullable=False)
    owner_email: Mapped[str] = mapped_column(String(255), nullable=False)
    owner_token: Mapped[str] = mapped_column(String(36), nullable=False)
    proposal: Mapped[dict] = mapped_column(JSONB, nullable=False)
    receipts: Mapped[dict | None] = mapped_column(JSONB, nullable=True)
