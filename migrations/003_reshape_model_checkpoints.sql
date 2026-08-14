-- The original `model_checkpoints` table was never written to, and its shape
-- does not fit what a training checkpoint is: it required a `task_id`
-- referencing the REST API's `tasks` table, which would have forced a bogus
-- task row into existence for every checkpoint. Nothing depended on the old
-- shape, so it is replaced outright rather than migrated.
--
-- This file is DESTRUCTIVE and is applied conditionally: `run_migrations` only
-- executes it when the legacy `task_id` column is still present. Running it on
-- an already-reshaped table would delete every saved checkpoint.
DROP TABLE IF EXISTS model_checkpoints;

CREATE TABLE IF NOT EXISTS model_checkpoints (
    checkpoint_id UUID PRIMARY KEY,
    -- Fingerprint of the network shape and dataset. Parameters are only
    -- meaningful for the model that produced them, so a configuration change
    -- starts a new run rather than resuming into a mismatched vector.
    model_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    -- Encrypted JSON of the parameter vector.
    parameters BYTEA NOT NULL,
    loss REAL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS model_checkpoints_by_run
    ON model_checkpoints (model_id, epoch DESC);
