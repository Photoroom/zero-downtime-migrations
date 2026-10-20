"""Authoritative PostgreSQL records for delivered v4 Tool generations."""

from datetime import datetime
from uuid import UUID

from sqlalchemy import CheckConstraint, ForeignKey, Index, String, UniqueConstraint, func
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import Mapped, mapped_column

from app.persistence.models import Base, BigInt


class AIToolGeneration(Base):
    """Immutable result provenance and owner-scoped primary asset location."""

    __tablename__ = "ai_tool_generations"
    __table_args__ = (UniqueConstraint("generation_id", name="uq_ai_tool_generations_generation_id"),)

    id: Mapped[UUID] = mapped_column(primary_key=True)
    generation_id: Mapped[str] = mapped_column(String(255))
    user_id: Mapped[str] = mapped_column(String(255))
    space_id: Mapped[str] = mapped_column(String(255))
    tool_id: Mapped[str] = mapped_column(String(255))
    tool_version: Mapped[int]
    delivery_state: Mapped[str] = mapped_column(String(32), default="not_delivered")
    primary_result_identifier: Mapped[str] = mapped_column(String(255))
    primary_result_gcs_uri: Mapped[str] = mapped_column(String(2048))
    primary_result_content_type: Mapped[str] = mapped_column(String(255))
    thumbnail_gcs_uri: Mapped[str | None] = mapped_column(String(2048))
    input_references: Mapped[list[dict]] = mapped_column(JSONB(none_as_null=True), default=list)
    created_at: Mapped[datetime] = mapped_column(server_default=func.now())
    delivered_at: Mapped[datetime | None]
    refunded_at: Mapped[datetime | None]


class AIToolGenerationProvenance(Base):
    """Signed watermark evidence for one delivered asset, with no identity fields."""

    __tablename__ = "ai_tool_generation_provenance"
    __table_args__ = (
        UniqueConstraint("generation_uuid", name="uq_ai_tool_generation_provenance_generation_uuid"),
        CheckConstraint("delivered_asset_sha256 ~ '^[0-9a-f]{64}$'", name="ck_ai_tool_generation_provenance_sha256"),
        CheckConstraint(
            "(manifest_gcs_uri IS NULL AND manifest_generation IS NULL AND manifest_sha256 IS NULL AND manifest_byte_length IS NULL) OR "
            "(manifest_gcs_uri IS NOT NULL AND manifest_generation IS NOT NULL AND manifest_sha256 IS NOT NULL AND manifest_byte_length IS NOT NULL "
            "AND manifest_gcs_uri ~ '^gs://[^/]+/c2pa/v1/.+\\.c2pa$' AND manifest_generation > 0 "
            "AND manifest_sha256 ~ '^[0-9a-f]{64}$' AND manifest_byte_length > 0)",
            name="ck_ai_tool_generation_provenance_manifest_object",
        ),
        Index("ix_ai_tool_generation_provenance_asset_sha256", "delivered_asset_sha256"),
        Index("uq_ai_tool_generation_provenance_output_identity", "output_identity", unique=True),
    )

    watermark_id: Mapped[UUID] = mapped_column(primary_key=True)
    generation_uuid: Mapped[UUID | None] = mapped_column(
        ForeignKey("ai_tool_generations.id", ondelete="SET NULL", name="fk_ai_tool_generation_provenance_generation_uuid"),
    )
    output_identity: Mapped[str | None] = mapped_column(String(255))
    clean_master_gcs_uri: Mapped[str | None] = mapped_column(String(2048))
    delivered_asset_gcs_uri: Mapped[str | None] = mapped_column(String(2048))
    delivery_content_type: Mapped[str | None] = mapped_column(String(255))
    manifest_gcs_uri: Mapped[str | None]
    manifest_generation: Mapped[BigInt | None]
    manifest_sha256: Mapped[str | None] = mapped_column(String(64))
    manifest_byte_length: Mapped[BigInt | None]
    delivered_asset_sha256: Mapped[str] = mapped_column(String(64))
    created_at: Mapped[datetime] = mapped_column(server_default=func.now())
