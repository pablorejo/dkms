-- Bump default Quditto rate_r0 from 120 keys/min to 120000 keys/min
-- (= 2000 keys/s at distance 0). Only changes the column default so
-- new rows inserted without an explicit value pick up the higher rate.
--
-- NOTE: we intentionally do NOT rewrite existing rows. Seeds from
-- topology JSON (``simulation-3.topology.json`` et al.) pass the
-- configured rate explicitly, and clobbering them to 120000 caused a
-- 1000× mismatch between the DKMS token bucket and the Quditto
-- simulator's actual pair-sync rate, starving buffer refills with
-- "dec_keys retry budget exhausted" 503s.
-- Target: PostgreSQL.

ALTER TABLE kme
    ALTER COLUMN channel_quditto_rate_r0 SET DEFAULT 120000.0;
