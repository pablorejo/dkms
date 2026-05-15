-- Incremental migration for Quditto rate-model link parameters.
-- Target: PostgreSQL.

ALTER TABLE kme
    ADD COLUMN IF NOT EXISTS channel_quditto_rate_r0 DOUBLE PRECISION NOT NULL DEFAULT 120000.0;

ALTER TABLE kme
    ADD COLUMN IF NOT EXISTS channel_quditto_rate_alpha DOUBLE PRECISION NOT NULL DEFAULT 0.2;
