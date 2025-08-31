CREATE TABLE IF NOT EXISTS model_checkpoints (
    checkpoint_id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(task_id),
    checkpoint BYTES,
    created_at TIMESTAMPTZ DEFAULT now()
);

ALTER TABLE model_checkpoints CONFIGURE ZONE USING num_replicas = 3;
