CREATE TABLE IF NOT EXISTS tasks (
    task_id UUID PRIMARY KEY,
    task_type STRING NOT NULL,
    data STRING NOT NULL,
    created_at TIMESTAMPTZ DEFAULT now()
);

-- Ensure column exists for deployments that ran the previous schema.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS task_type STRING NOT NULL DEFAULT 'ParameterPull';

-- Configure replication across regions (requires enterprise features on real clusters).
ALTER TABLE tasks CONFIGURE ZONE USING num_replicas = 3;
