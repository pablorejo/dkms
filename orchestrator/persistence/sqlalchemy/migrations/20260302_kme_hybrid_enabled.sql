-- Incremental migration for HYBRID link support in KME.
-- Target: PostgreSQL.

ALTER TABLE kme
    ADD COLUMN IF NOT EXISTS hybrid_enabled BOOLEAN NOT NULL DEFAULT FALSE;
