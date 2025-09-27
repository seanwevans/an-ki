CREATE TABLE IF NOT EXISTS tasks (
    task_id UUID PRIMARY KEY,
    task_type TEXT NOT NULL,
    data TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT now()
);

