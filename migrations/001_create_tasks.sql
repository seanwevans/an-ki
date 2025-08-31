CREATE TABLE IF NOT EXISTS tasks (
    task_id UUID PRIMARY KEY,
    data STRING NOT NULL,
    created_at TIMESTAMPTZ DEFAULT now()
);

-- Configure replication across regions (requires enterprise features on real clusters).
ALTER TABLE tasks CONFIGURE ZONE USING num_replicas = 3;
