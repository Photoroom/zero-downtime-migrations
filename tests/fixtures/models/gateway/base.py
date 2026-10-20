"""SQLAlchemy declarative base and shared column contracts for persistence models."""

import uuid
from collections.abc import Callable, Sequence
from contextvars import ContextVar
from datetime import datetime
from typing import Annotated, Any, ClassVar, LiteralString, NamedTuple, cast

from pydantic import BaseModel, PlainValidator, TypeAdapter, ValidationError
from pydantic_core import InitErrorDetails, PydanticCustomError
from sqlalchemy import UUID, BigInteger, DateTime, Identity, Result, Row, TypeDecorator, event, func
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import DeclarativeBase, Mapped, ORMExecuteState, Session, mapped_column, registry

BigIntPk = Annotated[int, mapped_column(BigInteger, Identity(), primary_key=True)]
"""Python int, persisted as BigInteger. Used as primary key."""
UuidPk = Annotated[uuid.UUID, mapped_column(UUID, primary_key=True, default=uuid.uuid4)]
"""Python uuid4, Used as primary key"""

BigInt = Annotated[int, mapped_column(BigInteger)]
"""Python int, persisted as BigInteger."""


# Utilize registry instead of `type_annotation_map` so that we can add freezegun types in tests
sql_orm_registry = registry(
    type_annotation_map={
        # By default, always store datetime with timezone.
        datetime: DateTime(timezone=True),
        # By default, dicts are stored as JSONB.
        dict: JSONB(none_as_null=True),
        dict[str, str]: JSONB(none_as_null=True),
        list: JSONB(none_as_null=True),
        list[str]: JSONB(none_as_null=True),
    },
)


class Base(DeclarativeBase):
    registry: ClassVar = sql_orm_registry


class TimestampMixin:
    """Adds timezone-aware creation and modification timestamps."""

    created_at: Mapped[datetime] = mapped_column(server_default=func.now())
    updated_at: Mapped[datetime] = mapped_column(server_default=func.now(), onupdate=func.now())


class SoftDeleteMixin:
    """Adds an opt-in timezone-aware deletion timestamp."""

    deleted_at: Mapped[datetime | None]


