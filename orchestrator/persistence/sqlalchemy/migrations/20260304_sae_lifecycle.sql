-- SAE lifecycle + certificate management support.
-- Target: PostgreSQL.

DO $$
BEGIN
    CREATE TYPE sae_status AS ENUM ('pending_cert', 'active', 'revoked', 'expired');
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

ALTER TABLE sae
    ALTER COLUMN sdn_id DROP NOT NULL;

ALTER TABLE sae
    ALTER COLUMN tls_id DROP NOT NULL;

ALTER TABLE sae
    ALTER COLUMN agent_dkms_id DROP NOT NULL;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS sae_id TEXT;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS display_name TEXT;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS owner_user_id INTEGER;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS simulation_id INTEGER;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS dkms_id INTEGER;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS status sae_status NOT NULL DEFAULT 'pending_cert';

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS cert_serial TEXT;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS cert_fingerprint TEXT;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS cert_subject TEXT;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS cert_not_before TIMESTAMPTZ;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS cert_not_after TIMESTAMPTZ;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS revoked_at TIMESTAMPTZ;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS revocation_reason TEXT;

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

ALTER TABLE sae
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'fk_sae_owner_user'
    ) THEN
        ALTER TABLE sae
            ADD CONSTRAINT fk_sae_owner_user
            FOREIGN KEY (owner_user_id)
            REFERENCES "user"(id)
            ON UPDATE CASCADE
            ON DELETE SET NULL;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'fk_sae_simulation'
    ) THEN
        ALTER TABLE sae
            ADD CONSTRAINT fk_sae_simulation
            FOREIGN KEY (simulation_id)
            REFERENCES simulation(id)
            ON UPDATE CASCADE
            ON DELETE SET NULL;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'fk_sae_dkms'
    ) THEN
        ALTER TABLE sae
            ADD CONSTRAINT fk_sae_dkms
            FOREIGN KEY (dkms_id)
            REFERENCES dkms(id)
            ON UPDATE CASCADE
            ON DELETE SET NULL;
    END IF;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS uq_sae_sae_id
ON sae(sae_id)
WHERE sae_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ix_sae_owner_user_id ON sae(owner_user_id);
CREATE INDEX IF NOT EXISTS ix_sae_simulation_id ON sae(simulation_id);
CREATE INDEX IF NOT EXISTS ix_sae_dkms_id ON sae(dkms_id);
CREATE INDEX IF NOT EXISTS ix_sae_cert_fingerprint ON sae(cert_fingerprint);
CREATE INDEX IF NOT EXISTS ix_sae_status ON sae(status);
CREATE INDEX IF NOT EXISTS ix_sae_fingerprint_status ON sae(cert_fingerprint, status);
