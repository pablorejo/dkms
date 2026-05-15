-- SAE identifiers are unique per simulation (simulation_id + sae_id), not globally.
-- Target: PostgreSQL.

-- Drop legacy global unique index if present.
DROP INDEX IF EXISTS uq_sae_sae_id;

-- Drop implicit unique constraint created by old schema definitions (if present).
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'sae_sae_id_key'
          AND conrelid = 'sae'::regclass
    ) THEN
        ALTER TABLE sae DROP CONSTRAINT sae_sae_id_key;
    END IF;
END $$;

-- Enforce uniqueness only inside each simulation.
CREATE UNIQUE INDEX IF NOT EXISTS uq_sae_simulation_sae_id
ON sae(simulation_id, sae_id)
WHERE simulation_id IS NOT NULL AND sae_id IS NOT NULL;
