-- Incremental migration for web-orchestrator integration.
-- Target: PostgreSQL.

ALTER TABLE simulation
    ADD COLUMN IF NOT EXISTS editor_topology_json TEXT;

ALTER TABLE simulation
    ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

ALTER TABLE simulation
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

ALTER TABLE sdn
    ADD COLUMN IF NOT EXISTS type_http TEXT NOT NULL DEFAULT 'http';

ALTER TABLE kme
    ADD COLUMN IF NOT EXISTS channel_quditto_max_buffer_size INTEGER NOT NULL DEFAULT 100;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_type t
        WHERE t.typname = 'simulation_run_status'
    ) THEN
        CREATE TYPE simulation_run_status AS ENUM ('QUEUED', 'RUNNING', 'DONE', 'FAILED');
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS simulation_run (
    id SERIAL PRIMARY KEY,
    simulation_id INTEGER NOT NULL REFERENCES simulation(id) ON UPDATE CASCADE ON DELETE CASCADE,
    status simulation_run_status NOT NULL,
    message TEXT NULL,
    queued_at TIMESTAMPTZ NULL,
    started_at TIMESTAMPTZ NULL,
    finished_at TIMESTAMPTZ NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_simulation_run_simulation_id ON simulation_run(simulation_id);
CREATE INDEX IF NOT EXISTS idx_simulation_run_created_at ON simulation_run(created_at);
