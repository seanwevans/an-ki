CREATE TABLE IF NOT EXISTS model_checkpoints (
    checkpoint_id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(task_id),
    checkpoint BYTEA,
    created_at TIMESTAMPTZ DEFAULT now()
);
